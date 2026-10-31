use std::sync::{Arc, Mutex};
use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::Preimage;
use ark::{ProtocolEncoding, VtxoPolicy};
use ark_testing::{btc, sat, TestContext};
use bark::{OpenWalletArgs, WalletSeed};
use bark::actions::offboard::Progress;
use bark::onchain::OnchainWallet;
use bark::persist::{BarkPersister, sqlite::SqliteClient};
use bark::vtxo::{VtxoLockHolder, VtxoState};
use ark_testing::daemon::captaind::{self, ArkClient};
use bitcoin::{Transaction, Txid};
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::BlockHeight;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::{Db, Tx};
use server_rpc::protos;
use tokio::sync::{Notify, oneshot};

/// Park an SDK request after prepare/signing, before the server's
/// durable registration. The request uses signed transactions.
#[derive(Clone)]
struct HoldFinish {
	arrived: Arc<Mutex<Option<oneshot::Sender<protos::FinishOffboardRequest>>>>,
	release: Arc<Notify>,
	response: Arc<Mutex<Option<Result<protos::FinishOffboardResponse, tonic::Status>>>>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for HoldFinish {
	async fn finish_offboard(
		&self, upstream: &mut ArkClient, request: protos::FinishOffboardRequest,
	) -> Result<protos::FinishOffboardResponse, tonic::Status> {
		let arrived = self.arrived.lock().unwrap().take();
		if let Some(arrived) = arrived {
			arrived.send(request.clone()).expect("race controller still alive");
			self.release.notified().await;
		}
		let result = upstream.finish_offboard(request).await.map(tonic::Response::into_inner);
		self.response.lock().unwrap().get_or_insert_with( || result.clone());
		result
	}
}

async fn wait_for_query_lock(tx: &Tx<'_>, prefix: &str) -> anyhow::Result<()> {
	tokio::time::timeout(Duration::from_secs(15), async {
		loop {
			tx.query_one("
				SELECT pg_stat_clear_snapshot()
			", &[]).await?;
			let waiting = tx.query_one("
				SELECT EXISTS(
					SELECT 1
					FROM pg_stat_activity
					WHERE datname=current_database()
					AND wait_event_type='Lock'
					AND ltrim(query, chr(9) || chr(10) || chr(13) || ' ') LIKE $1)
			", &[&prefix]).await?.get::<_, bool>(0);
			if waiting { return Ok::<_, anyhow::Error>(()); }
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	}).await.expect("real competing operation never reached its PostgreSQL row lock")
}

async fn offboard_race(offboard_wins: bool, finality: bool) {
	let name = if finality { "server/expiry_native_offboard_fallback_finality" }
		else if offboard_wins { "server/expiry_native_offboard_wins" }
		else { "server/expiry_native_decision_wins" };
	let ctx = TestContext::new(name).await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().funded(btc(1))
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24)).watchmand().create().await;
	let (send, arrived) = oneshot::channel();
	let gate = HoldFinish {
		arrived: Arc::new(Mutex::new(Some(send))), release: Arc::new(Notify::new()),
		response: Arc::new(Mutex::new(None))
	};
	let proxy = srv.start_proxy_no_mailbox(gate.clone()).await;
	// This actor follows the test's explicit RPC/height schedule.
	let builder = ctx.bark_sdk("owner", &proxy).cfg(move |config| {
		config.daemon_manual_sync = true;
		if finality {
			config.vtxo_refresh_expiry_threshold = BlockDelta::new(4);
			config.vtxo_exit_margin = BlockDelta::new(2);
		}
	});
	let builder = if finality { builder.funded(sat(500_000)) } else { builder };
	let wallet = Arc::new(builder.boarded(sat(100_000)).create().await);
	let coin = wallet.spendable_vtxos().await.unwrap().pop().unwrap();
	let id = coin.id();
	let destination = wallet.fallback_script();
	let native_balance = wallet.history().await.unwrap().iter()
		.map(|entry| entry.effective_balance.to_sat()).sum::<i64>();
	assert_eq!(native_balance, 100_000, "the genuine board credit must be retained");
	let offboard_address = ctx.bitcoind().get_new_address();
	let funding = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(funding, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let sender = wallet.clone();
	let address = offboard_address.clone();
	let operation = tokio::spawn(async move { sender.offboard_vtxos([id], address).await });
	let request = tokio::time::timeout(Duration::from_secs(15), arrived).await.unwrap().unwrap();
	let offboard_txid = Txid::from_slice(&request.offboard_txid).unwrap();
	let wallet_db = SqliteClient::open(ctx.datadir.join("owner/db.sqlite")).unwrap();
	let prepared = wallet_db.get_all_wallet_action_checkpoints().await.unwrap().into_iter()
		.filter_map(|checkpoint| checkpoint.into_offboard())
		.find(|offboard| matches!(&offboard.progress,
			Progress::OffboardTxPrepared { offboard_tx, .. } if offboard_tx.compute_txid() == offboard_txid))
		.expect("the genuine prepared offboard must be durable before its RPC");
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	// The old watchman view prevents a decision while Core and captaind
	// reach expiry. No chain acknowledgement or financial row is fabricated.
	srv.watchmand().stop().await.unwrap();
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	if offboard_wins {
		db.write(async |tx| {
			tx.query_one("
				SELECT vtxo_id
				FROM vtxo
				WHERE vtxo_id=$1
				FOR UPDATE
			", &[&id.to_string()]).await?;
			gate.release.notify_one();
			wait_for_query_lock(tx, "SELECT vtxo_id FROM vtxo WHERE vtxo_id = ANY%").await?;
			// The native offboard is first in the real row-lock queue. The
			// completed watchman now permits a real allocation to queue too.
			srv.watchmand().start().await.unwrap();
			wait_for_query_lock(tx, "SELECT vtxo_id,updated_at,spend_state::TEXT,%").await?;
			Ok(())
		}).await.unwrap();
		let actual = tokio::time::timeout(Duration::from_secs(20), operation).await.unwrap().unwrap().unwrap().txid;
		assert_eq!(actual, offboard_txid);
		let response = gate.response.lock().unwrap().clone().unwrap().unwrap();
		let signed: Transaction = deserialize(&response.signed_offboard_tx).unwrap();
		assert_eq!(ctx.bitcoind().await_transaction(actual).await, signed);
		let delivered = signed.output.iter().find(|o| o.script_pubkey == offboard_address.script_pubkey()).unwrap().value;
		let fee = db.read(async |tx| Ok(tx.query_one("
			SELECT user_fee_sat
			FROM offboards
			WHERE txid=$1
		", &[&actual.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(delivered + sat(u64::try_from(fee).unwrap()), coin.amount());
		ctx.generate_blocks(6).await;
		assert_eq!(ctx.bitcoind().get_received_by_address(&offboard_address), delivered);
		let customer = db.read(async |tx| Ok(tx.query_one("
			SELECT COUNT(*)
			FROM expiry_allocation_coin c
			JOIN expiry_entitlement e ON e.id=c.entitlement_id
			WHERE e.vtxo_id=$1
			AND c.disposition='customer'
		", &[&id.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(customer, 0, "the binding offboard must not also receive a fallback payment");
	} else {
		srv.watchmand().start().await.unwrap();
		let parent: Transaction = tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				let row = db.read(async |tx| Ok(tx.query_opt("
					SELECT a.sweep_tx
					FROM expiry_allocation a
					JOIN expiry_allocation_coin c ON c.allocation_id=a.id
					JOIN expiry_entitlement e ON e.id=c.entitlement_id
					WHERE e.vtxo_id=$1
					AND c.active
					AND a.broadcast_at IS NOT NULL
				", &[&id.to_string()]).await?)).await.unwrap();
				if let Some(row) = row { break deserialize(row.get("sweep_tx")).unwrap(); }
				tokio::time::sleep(Duration::from_millis(25)).await;
			}
		}).await.expect("actual payout decision did not publish");
		gate.release.notify_one();
		let error = tokio::time::timeout(Duration::from_secs(20), operation).await.unwrap().unwrap().unwrap_err();
		assert_eq!(bark::FallbackRefusal::from_error(&error), Some(bark::FallbackRefusal::DecidedForFallback));
		let status = gate.response.lock().unwrap().clone().unwrap().unwrap_err();
		assert_eq!(status.code(), tonic::Code::InvalidArgument);
		assert_eq!(status.metadata().get("abandon-reason").unwrap(), "decided_for_fallback");
		assert_eq!(wallet.offboard_checkpoint(&prepared.id).await.unwrap(), Some(prepared.clone()),
			"an admission fence must retain the original prepared checkpoint");
		assert_eq!(wallet_db.get_wallet_vtxo(id).await.unwrap().unwrap().state,
			VtxoState::Locked { holder: Some(VtxoLockHolder::Action { id: prepared.id.clone() }) },
			"a server refusal alone must not release the native action holder");
		let state = db.read(async |tx| tx.get_user_vtxos_by_id(&[id]).await).await.unwrap();
		assert_eq!(state[0].offboarded_in, None);
		let persisted = db.read(async |tx| Ok(tx.query_one("
			SELECT COUNT(*)
			FROM offboards
			WHERE txid=$1
		", &[&offboard_txid.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(persisted, 0, "the actual registration transaction must roll back its offboard insert");
		assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
		let (index, output) = parent.output.iter().enumerate().find(|(_, o)| o.script_pubkey == destination).unwrap();
		assert_eq!(output.value, coin.amount() - sat(1_000));
		let movement = match &prepared.progress {
			Progress::OffboardTxPrepared { movement_id, .. } => *movement_id,
			_ => unreachable!(),
		};
		if finality {
			let cancelled = wallet_db.get_movement_by_id(movement).await.unwrap();
			assert_eq!(cancelled.status, bark::movement::MovementStatus::Canceled);
			assert_eq!(cancelled.effective_balance, bitcoin::SignedAmount::ZERO);
			assert_eq!(cancelled.offchain_fee, bitcoin::Amount::ZERO);
			wallet.sync_fallback_payouts().await.unwrap();
			assert_eq!(wallet.offboard_checkpoint(&prepared.id).await.unwrap(), Some(prepared.clone()));
			let history = wallet.history().await.unwrap();
			assert_eq!(history.iter().map(|entry| entry.effective_balance.to_sat()).sum::<i64>(), native_balance,
				"a refused offboard must not debit the genuine board credit");
		}
		ctx.generate_blocks(6).await;
		assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
		assert_eq!(ctx.bitcoind().get_received_by_address(&offboard_address), bitcoin::Amount::ZERO);
		if finality {
			let signed = wallet.get_full_vtxo(id).await.unwrap();
			wallet.chain().invalidate_caches().await;
			wallet.sync_fallback_payouts().await.unwrap();
			srv.watchmand().stop().await.unwrap();
			srv.stop().await.unwrap();
			wallet.sync_pending_offboards().await.unwrap();
			assert_eq!(wallet.offboard_checkpoint(&prepared.id).await.unwrap(), Some(prepared.clone()),
				"without a fresh authenticated input status, retain the cancelled intent rather than infer from Bitcoin");
			assert_eq!(wallet_db.get_wallet_vtxo(id).await.unwrap().unwrap().state, VtxoState::Spent);
			assert_eq!(wallet.get_full_vtxo(id).await.unwrap(), signed, "payment must retain signed recovery material");
			let history = wallet.history().await.unwrap();
			assert_eq!(history.iter().map(|entry| entry.effective_balance.to_sat()).sum::<i64>(), native_balance);
			let cancelled = history.iter().find(|entry| entry.id == movement).unwrap();
			assert_eq!(cancelled.status, bark::movement::MovementStatus::Canceled);
			assert_eq!(cancelled.effective_balance, bitcoin::SignedAmount::ZERO);
			assert_eq!(cancelled.offchain_fee, bitcoin::Amount::ZERO);
			let rpc = ctx.bitcoind().sync_client();
			let confirmed = rpc.custom_get_raw_transaction_info(parent.compute_txid(), None).unwrap().unwrap();
			rpc.invalidate_block(&confirmed.blockhash.unwrap()).unwrap();
			wallet.chain().invalidate_caches().await;
			assert!(wallet.sync_fallback_payouts().await.is_err());
			wallet.sync_onchain().await.unwrap();
			assert!(wallet.exit_mgr().get_exit_vtxo(id).await.is_none(), "fallback reconciliation must retain upstream exit scheduling");
			assert_eq!(wallet.get_full_vtxo(id).await.unwrap(), signed);
			let pending = wallet.balance().await.unwrap();
			assert_eq!(wallet_db.get_wallet_vtxo(id).await.unwrap().unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
			assert_eq!(pending.total(), bitcoin::Amount::ZERO);
			assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
			assert!(wallet.lock_vtxos([id], None).await.is_err());
			assert_eq!(signed.amount(), coin.amount());
			let history = wallet.history().await.unwrap();
			let cancelled = history.iter().find(|entry| entry.id == movement).unwrap();
			assert_eq!(cancelled.status, bark::movement::MovementStatus::Canceled);
			assert_eq!(cancelled.effective_balance, bitcoin::SignedAmount::ZERO);
			assert_eq!(cancelled.offchain_fee, bitcoin::Amount::ZERO);
			assert_eq!(history.iter().map(|entry| entry.effective_balance.to_sat()).sum::<i64>(), native_balance);

			// Keep the offline cut above unchanged. Bring back the SAME server
			// and wallet stores; listener ports are intentionally reassigned.
			let allocations = db.read(async |tx| Ok(tx.query("
				SELECT a.id,a.sweep_tx,c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat
				FROM expiry_allocation a
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id
				JOIN expiry_entitlement e ON e.id=c.entitlement_id
				WHERE e.vtxo_id=$1 ORDER BY a.id
			", &[&id.to_string()]).await?.into_iter().map(|row|
				(row.get::<_, i64>(0),row.get::<_, Vec<u8>>(1),row.get::<_, i64>(2),
				 row.get::<_, i64>(3),row.get::<_, i64>(4),row.get::<_, i64>(5))).collect::<Vec<_>>())).await.unwrap();
			assert_eq!(allocations.len(), 1);
			assert_eq!((allocations[0].2,allocations[0].3,allocations[0].4,allocations[0].5), (100_000,1_000,99_000,0));
			wallet.stop_daemon_wait().await.unwrap();
			drop(wallet);
			srv.start().await.unwrap();
			srv.watchmand().start().await.unwrap();
			super::expiry_test::rebuilt(&ctx, &srv, &db).await;
			let retry_gate = HoldFinish {
				arrived: Arc::new(Mutex::new(None)), release: Arc::new(Notify::new()),
				response: Arc::new(Mutex::new(None)),
			};
			let retry_proxy = srv.start_proxy_no_mailbox(retry_gate.clone()).await;
			let mnemonic: bip39::Mnemonic = tokio::fs::read_to_string(ctx.datadir.join("owner/mnemonic"))
				.await.unwrap().trim().parse().unwrap();
			let onchain = OnchainWallet::load_or_create(bitcoin::Network::Regtest,
				mnemonic.to_seed(""), Arc::new(wallet_db.clone())).await.unwrap();
			let mut config: bark::Config = toml::from_str(&tokio::fs::read_to_string(
				ctx.datadir.join("owner/config.toml")).await.unwrap()).unwrap();
			config.server_address = retry_proxy.ark_url();
			let wallet = bark::Wallet::open(bitcoin::Network::Regtest,
				WalletSeed::new_from_mnemonic(bitcoin::Network::Regtest, &mnemonic), config,
				OpenWalletArgs { persister: Some(Arc::new(wallet_db.clone())),
					onchain: Some(Arc::new(tokio::sync::RwLock::new(onchain))), run_daemon: false,
					..Default::default() }).await.unwrap();
			assert_eq!(wallet.fallback_script(), destination);
			wallet.sync_fallback_payouts().await.unwrap();
			assert_eq!(wallet.get_vtxo_by_id(id).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
			assert_eq!(wallet.offboard_checkpoint(&prepared.id).await.unwrap(), Some(prepared.clone()));
			wallet.sync_pending_offboards().await.unwrap();
			let unknown = retry_gate.response.lock().unwrap().clone().unwrap().unwrap_err();
			assert_eq!(unknown.code(), tonic::Code::InvalidArgument);
			assert!(unknown.message().contains("unknown offboard txid"));
			let retry = wallet.offboard_checkpoint(&prepared.id).await.unwrap().unwrap();
			assert!(matches!(&retry.progress, Progress::ReadyForOffboard { prior_txid: Some(prior), .. } if *prior == offboard_txid));
			let mut expected_retry = prepared.clone();
			expected_retry.progress = retry.progress.clone();
			assert_eq!(retry, expected_retry, "session loss must retain the exact original intent and committed fee");
			wallet.sync_pending_offboards().await.unwrap();
			assert_eq!(wallet.offboard_checkpoint(&prepared.id).await.unwrap(), Some(retry));
			assert_eq!(wallet_db.get_movement_by_id(movement).await.unwrap().status, bark::movement::MovementStatus::Canceled);
			assert!(wallet.spendable_vtxos().await.unwrap().is_empty());

			// Reconsider the ORIGINAL confirmation branch: no new funding,
			// payment, fee or mining is needed to restore this same parent to N6.
			rpc.reconsider_block(&confirmed.blockhash.unwrap()).unwrap();
			super::expiry_test::rebuilt(&ctx, &srv, &db).await;
			wallet.chain().invalidate_caches().await;
			wallet.sync_fallback_payouts().await.unwrap();
			assert_eq!(wallet.get_vtxo_by_id(id).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
			let mempool = ctx.bitcoind().sync_client().get_raw_mempool().unwrap();
			wallet.sync_pending_offboards().await.unwrap();
			assert!(wallet.offboard_checkpoint(&prepared.id).await.unwrap().is_none(),
				"authenticated terminal fallback must finish the superseded offboard after its session was lost");
			let failed = wallet_db.get_movement_by_id(movement).await.unwrap();
			assert_eq!(failed.status, bark::movement::MovementStatus::Failed);
			assert_eq!(failed.effective_balance, bitcoin::SignedAmount::ZERO);
			assert_eq!(failed.offchain_fee, bitcoin::Amount::ZERO);
			assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
			assert!(wallet.lock_vtxos([id], None).await.is_err());
			assert_eq!(wallet_db.get_wallet_vtxo(id).await.unwrap().unwrap().state, VtxoState::Spent);
			assert_eq!(wallet.get_full_vtxo(id).await.unwrap().serialize(), signed.serialize());
			let history = wallet.history().await.unwrap();
			assert_eq!(history.iter().map(|entry| entry.effective_balance.to_sat()).sum::<i64>(), native_balance);
			wallet.sync_pending_offboards().await.unwrap();
			wallet.sync_fallback_payouts().await.unwrap();
			assert_eq!(wallet.history().await.unwrap(), history, "terminal offboard cleanup is idempotent");
			assert_eq!(ctx.bitcoind().sync_client().get_raw_mempool().unwrap(), mempool);
			assert_eq!(ctx.bitcoind().get_received_by_address(&offboard_address), bitcoin::Amount::ZERO);
			let paid = rpc.get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap();
			assert!(paid.confirmations >= 6);
			assert_eq!(paid.value, sat(99_000));
			let after = db.read(async |tx| Ok(tx.query("
				SELECT a.id,a.sweep_tx,c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat
				FROM expiry_allocation a
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id
				JOIN expiry_entitlement e ON e.id=c.entitlement_id
				WHERE e.vtxo_id=$1 ORDER BY a.id
			", &[&id.to_string()]).await?.into_iter().map(|row|
				(row.get::<_, i64>(0),row.get::<_, Vec<u8>>(1),row.get::<_, i64>(2),
				 row.get::<_, i64>(3),row.get::<_, i64>(4),row.get::<_, i64>(5))).collect::<Vec<_>>())).await.unwrap();
			assert_eq!(after, allocations, "recovery must retain one original allocation, immutable parent and fee");
		}

	}
}

#[tokio::test]
async fn expiry_decision_wins_over_prepared_native_offboard() {
	offboard_race(false, false).await;
}

#[tokio::test]
async fn expiry_registered_native_offboard_wins_over_waiting_decision() {
	offboard_race(true, false).await;
}

/// A user returning after backing expiry can close a never-initiated grant
/// before its later HTLC deadline. This needs no configured Lightning node.
#[tokio::test]
async fn expiry_uninitiated_grant_can_revoke_after_backing_expiry() {
	let ctx = TestContext::new("server/expiry_post_backing_grant_revocation").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24)).watchmand().create().await;
	// Abandonment means this actor runs no automatic protection task.
	let wallet = ctx.bark_sdk("owner", &srv)
		.cfg(|config| config.daemon_manual_sync = true).boarded(sat(100_000)).create().await;
	let input = wallet.spendable_vtxos().await.unwrap().pop().unwrap();
	let input = wallet.get_full_vtxo(input.id()).await.unwrap();
	let key = wallet.get_vtxo_key(&input).await.unwrap();
	let destination = wallet.fallback_script();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	assert!(deadline > input.expiry_height());
	let hash = Preimage::from([91;32]).compute_payment_hash();
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input.clone()],
		VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).unwrap()
		.generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
	}).await.unwrap().into_inner();
	let grants = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(grants.len(), 1);
	wallet.register_vtxo_transactions_with_server(&grants).await.unwrap();
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(input.expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	assert!(expired < deadline);
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	assert_eq!(db.read(async |tx| Ok(tx.query_one("
		SELECT COUNT(*)
		FROM lightning_payment_attempt_htlc_vtxo
		WHERE vtxo_id=$1
	", &[&grants[0].id().to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([grants[0].clone()],
		VtxoPolicy::new_pubkey(key.public_key())).unwrap().generate_user_nonces(&[key]).unwrap();
	let response = rpc.request_lightning_pay_htlc_revocation(
		super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await,
	).await.unwrap().into_inner();
	let refunds = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(refunds.iter().map(|coin| coin.amount()).sum::<bitcoin::Amount>(), input.amount());
	wallet.register_vtxo_transactions_with_server(&refunds).await.unwrap();
	let funding = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(funding, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let parent: Transaction = tokio::time::timeout(Duration::from_secs(20), async {
		loop {
			let row = db.read(async |tx| Ok(tx.query_opt("
				SELECT a.sweep_tx
				FROM expiry_allocation a
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id
				JOIN expiry_entitlement e ON e.id=c.entitlement_id
				WHERE e.vtxo_id=$1
				AND c.active
				AND a.broadcast_at IS NOT NULL
			", &[&refunds[0].id().to_string()]).await?)).await.unwrap();
			if let Some(row) = row { break deserialize(row.get("sweep_tx")).unwrap(); }
			tokio::time::sleep(Duration::from_millis(25)).await;
		}
	}).await.expect("genuine post-expiry revocation did not permit its actual fallback payout");
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	let (index, output) = parent.output.iter().enumerate().find(|(_, o)| o.script_pubkey == destination).unwrap();
	assert_eq!(output.value, input.amount() - sat(1_000));
	ctx.generate_blocks(6).await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
	assert!((ctx.bitcoind().get_block_count().await as u32) < deadline.to_u32());
}

#[tokio::test]
async fn expiry_prepared_offboard_retains_principal_when_payout_reorgs() {
	offboard_race(false, true).await;
}
