//! A real server admission decision is not proof that the customer was paid.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use bark::persist::{BarkPersister, sqlite::SqliteClient};
use bark::vtxo::VtxoState;
use bitcoin::Amount;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::RpcApi;
use server::database::Db;

use ark::{ProtocolEncoding, VtxoId};
use ark_testing::{sat, TestContext};
use ark_testing::daemon::captaind;
use server_rpc::protos;

/// Declared response faults AFTER the real server authenticated the known
/// input and returned its actual Pending decision. No financial status is made up.
#[derive(Clone, Default)]
struct PendingStatusFault {
	target: Arc<Mutex<Option<VtxoId>>>,
	mode: Arc<AtomicU8>,
	foreign_owner: Arc<Mutex<Vec<u8>>>,
	single_faults: Arc<AtomicUsize>,
	batch_faults: Arc<AtomicUsize>,
}

impl PendingStatusFault {
	const MISSING: u8 = 1;
	const FOREIGN: u8 = 2;
	const STALLED: u8 = 3;
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for PendingStatusFault {
	async fn get_vtxo_status(&self, upstream: &mut captaind::ArkClient, request: protos::GetVtxoStatusRequest)
		-> Result<protos::GetVtxoStatusResponse, tonic::Status>
	{
		let target = *self.target.lock().unwrap();
		let targeted = target.is_some_and(|id| request.vtxo_id == id.to_bytes().to_vec());
		let mut response = upstream.get_vtxo_status(request).await?.into_inner();
		let mode = self.mode.load(Ordering::SeqCst);
		if targeted && mode != 0 {
			assert_eq!(response.spend_state, protos::VtxoSpendState::FallbackPending as i32);
			assert!(response.fallback_committed);
			self.single_faults.fetch_add(1, Ordering::SeqCst);
			if mode == Self::STALLED { std::future::pending::<()>().await; }
			if mode == Self::MISSING {
				return Err(tonic::Status::not_found("declared missing known-Pending status response"));
			}
			assert_eq!(mode, Self::FOREIGN);
			let foreign = self.foreign_owner.lock().unwrap().clone();
			assert_ne!(response.owner_mailbox, foreign);
			response.owner_mailbox = foreign;
		}
		Ok(response)
	}

	async fn get_vtxos_status_by_ids(&self, upstream: &mut captaind::ArkClient, request: protos::GetVtxosStatusByIdsRequest)
		-> Result<protos::GetVtxosStatusByIdsResponse, tonic::Status>
	{
		let target = *self.target.lock().unwrap();
		let targeted = target.is_some_and(|id| request.vtxos.iter().any(|query| query.vtxo_id == id.to_bytes().to_vec()));
		let mut response = upstream.get_vtxos_status_by_ids(request).await?.into_inner();
		let mode = self.mode.load(Ordering::SeqCst);
		if targeted && mode != 0 {
			let id = target.unwrap().to_bytes().to_vec();
			let index = response.vtxos.iter().position(|coin| coin.vtxo_id == id).expect("real server must return the known input");
			assert_eq!(response.vtxos[index].spend_state, protos::VtxoSpendState::FallbackPending as i32);
			assert!(response.vtxos[index].fallback_committed);
			self.batch_faults.fetch_add(1, Ordering::SeqCst);
			if mode == Self::STALLED { std::future::pending::<()>().await; }
			if mode == Self::MISSING {
				response.vtxos.remove(index);
				assert!(!response.missing_vtxo_ids.contains(&id));
				response.missing_vtxo_ids.push(id);
			} else {
				assert_eq!(mode, Self::FOREIGN);
				let foreign = self.foreign_owner.lock().unwrap().clone();
				assert_ne!(response.vtxos[index].owner_mailbox, foreign);
				response.vtxos[index].owner_mailbox = foreign;
			}
		}
		Ok(response)
	}
}

#[tokio::test]
async fn fallback_unfunded_decision_keeps_exit_material_until_actual_chain_payment() {
	let ctx = TestContext::new("bark_sdk/fallback_unfunded_protection").await;
	let srv = ctx.captaind("server").no_vtxo_pool()
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24))
		.watchmand().create().await;
	let fault = PendingStatusFault::default();
	let proxy = srv.start_proxy_no_mailbox(fault.clone()).await;
	let wallet = ctx.bark_sdk("owner", &proxy).cfg(|cfg| cfg.daemon_manual_sync = true)
		.boarded(sat(100_000)).create().await;
	let coin = wallet.spendable_vtxos().await.unwrap().remove(0);
	let original = wallet.get_full_vtxo(coin.id()).await.unwrap();
	// Start the real exit before the commitment is discovered, but publish
	// nothing. The later receipt must fence this existing abortable lifecycle.
	wallet.exit_mgr().start_exit_for_vtxos(&[coin.vtxo.clone()]).await.unwrap();
	let initial_exit = wallet.exit_mgr().get_exit_vtxo(coin.id()).await.unwrap().state().clone();
	assert!(initial_exit.is_cancelable());
	let exit_txids = original.transactions().map(|step| step.tx.compute_txid()).collect::<std::collections::BTreeSet<_>>();
	let registration = wallet.fallback_script();
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;

	// The watchman is deliberately unfunded. background readiness and
	// allocation create the fence; the test writes no ledger or receipt rows.
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |tx| Ok(tx.query_opt(
				"SELECT e.id FROM expiry_entitlement e WHERE e.vtxo_id=$1",
				&[&coin.id().to_string()],
			).await?)).await.unwrap();
			if row.is_some() { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual background allocation never decided the native coin");
	wallet.sync().await;
	let client_db = SqliteClient::open(ctx.datadir.join("owner/db.sqlite")).unwrap();
	assert_eq!(client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap().state, VtxoState::Locked { holder: None },
		"a committed decision fences the retained exit material until its canonical outcome");
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), original.serialize());
	assert!(wallet.spendable_vtxos().await.unwrap().is_empty(), "protected is not available for new payments");
	assert!(super::fallback_status_ids(wallet.all_vtxos().await.unwrap(), false).contains(&coin.id()));
	assert!(wallet.exit_mgr().start_exit_for_vtxos(&[coin.vtxo.clone()]).await.is_err());
	assert!(wallet.lock_vtxos([coin.id()], None).await.is_err());
	for _ in 0..2 {
		wallet.sync_exits().await.unwrap();
		let _ = wallet.progress_exits().await.unwrap();
		assert!(wallet.exit_mgr().exits_needing_cpfp().await.is_empty());
		assert_eq!(*wallet.exit_mgr().get_exit_vtxo(coin.id()).await.unwrap().state(), initial_exit);
		assert!(ctx.bitcoind().sync_client().get_raw_mempool().unwrap().iter().all(|txid| !exit_txids.contains(txid)),
			"learning a commitment must not publish the already-started exit");
	}
	let pending = wallet.history().await.unwrap();
	assert!(pending.iter().filter(|movement| movement.subsystem.name == "scuttle.fallback")
		.all(|movement| movement.status != bark::movement::MovementStatus::Successful));
	assert!(super::fallback_status_ids(client_db.get_all_vtxos().await.unwrap(), true).is_empty());
	let owed = wallet.balance().await.unwrap();
	assert_eq!(owed.spendable, Amount::ZERO);
	assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().amount(), coin.amount());
	assert_eq!(owed.total(), Amount::ZERO, "known committed principal is retained as signed coin/status, not another Ark amount");

	// An omitted, foreign or stalled status cannot forget a commitment already learned
	// from the actual allocation. Use the server's existing public key as a
	// valid but foreign point; no extra wallet, key or money is introduced.
	let foreign = wallet.require_ark_info().await.unwrap().mailbox_pubkey;
	assert_ne!(foreign, wallet.mailbox_identifier().as_pubkey());
	*fault.foreign_owner.lock().unwrap() = foreign.serialize().to_vec();
	*fault.target.lock().unwrap() = Some(coin.id());
	let checkpoints = serde_json::to_vec(&client_db.get_all_wallet_action_checkpoints().await.unwrap()).unwrap();
	for mode in [PendingStatusFault::MISSING, PendingStatusFault::FOREIGN, PendingStatusFault::STALLED] {
		fault.mode.store(mode, Ordering::SeqCst);
		let single = fault.single_faults.load(Ordering::SeqCst);
		let error = tokio::time::timeout(Duration::from_secs(15),
			wallet.trust_and_adopt_server_vtxo_status(coin.id())).await.unwrap().unwrap_err();
		assert_eq!(fault.single_faults.load(Ordering::SeqCst), single + 1);
		if mode == PendingStatusFault::MISSING {
			assert_eq!(error.downcast_ref::<tonic::Status>().unwrap().code(), tonic::Code::NotFound);
			assert!(format!("{error:#}").contains("declared missing known-Pending status response"));
		} else if mode == PendingStatusFault::STALLED {
			assert!(format!("{error:#}").contains("Ark server read deadline elapsed"));
		} else {
			assert!(format!("{error:#}").contains("server coin ownership does not match this mailbox"));
		}
		let batch = fault.batch_faults.load(Ordering::SeqCst);
		let result = tokio::time::timeout(Duration::from_secs(15), wallet.sync_fallback_payouts()).await.unwrap();
		assert_eq!(fault.batch_faults.load(Ordering::SeqCst), batch + 1);
		if mode != PendingStatusFault::FOREIGN {
			// The existing batch API represents omission or timeout as an absent map entry.
			// It is a non-decision, not an error or release of the known fence.
			result.unwrap();
		} else {
			assert!(format!("{:#}", result.unwrap_err()).contains("server coin ownership does not match this mailbox"));
		}
		wallet.unlock_vtxos([coin.id()], None).await.unwrap();
		let retained = client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap();
		assert_eq!(retained.fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
		assert_eq!(retained.state, VtxoState::Locked { holder: None });
		assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), original.serialize());
		assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
		assert!(wallet.lock_vtxos([coin.id()], None).await.is_err());
		assert!(wallet.exit_mgr().start_exit_for_vtxos(&[coin.vtxo.clone()]).await.is_err());
		assert_eq!(*wallet.exit_mgr().get_exit_vtxo(coin.id()).await.unwrap().state(), initial_exit);
		assert_eq!(wallet.balance().await.unwrap().total(), Amount::ZERO);
		assert_eq!(wallet.history().await.unwrap(), pending);
		assert_eq!(serde_json::to_vec(&client_db.get_all_wallet_action_checkpoints().await.unwrap()).unwrap(), checkpoints);
	}
	fault.mode.store(0, Ordering::SeqCst);
	wallet.sync_fallback_payouts().await.unwrap();
	assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));

	// A real subsidy now permits the existing unchanged payout to confirm.
	let address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let _payout = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			for txid in ctx.bitcoind().sync_client().get_raw_mempool().unwrap() {
				let tx = ctx.bitcoind().await_transaction(txid).await;
				if tx.output.iter().any(|output| output.script_pubkey == registration
					&& output.value == coin.amount() - sat(1_000)) { return txid; }
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual fallback did not broadcast after fee funding");
	let first_confirmation = ctx.generate_blocks(1).await;
	let confirmation_hash = ctx.bitcoind().sync_client().get_block_hash(first_confirmation.into()).unwrap();
	let confirmed = ctx.generate_blocks(5).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	wallet.chain().invalidate_caches().await;
	wallet.sync_fallback_payouts().await.unwrap();
	assert_eq!(client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap().state, VtxoState::Spent);
	let receipts = client_db.get_all_vtxos().await.unwrap();
	assert!(receipts.iter().any(|r| r.id() == coin.id() && r.fallback_status == Some(bark::vtxo::FallbackStatus::Spent)));
	assert!(!super::fallback_status_ids(wallet.all_vtxos().await.unwrap(), false).contains(&coin.id()));
	let paid_balance = wallet.balance().await.unwrap();
	assert_eq!(paid_balance.spendable, Amount::ZERO);
	assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
	assert_eq!(paid_balance.total(), Amount::ZERO, "the actual settled onchain payout is not counted again as Ark value");

	// Bitcoin learns the reorg offline; Ark keeps the cached commitment
	// until an authenticated status update is available.
	srv.stop().await.unwrap();
	ctx.bitcoind().sync_client().invalidate_block(&confirmation_hash).unwrap();
	wallet.chain().invalidate_caches().await;
	let _ = wallet.sync_fallback_payouts().await;
	wallet.sync_onchain().await.unwrap();
	assert!(ctx.bitcoind().sync_client().get_block_header_info(&confirmation_hash).unwrap().confirmations < 0);
	assert_eq!(client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
	assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
	assert!(wallet.lock_vtxos([coin.id()], None).await.is_err());
	assert!(wallet.exit_mgr().start_exit_for_vtxos(&[coin.vtxo.clone()]).await.is_err());
	assert!(!matches!(wallet.refresh_vtxos([coin.id()]).await, Ok(Some(_))), "an offline reorg cannot start another Ark round");
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), original.serialize());
	// The client-only offline cut above is not an authenticated Pending
	// projection. Make the same real fork visible to the server's Core.
	srv.bitcoind().sync_client().invalidate_block(&confirmation_hash).unwrap();
	let fork = bitcoin_ext::BlockRef {
		height: bitcoin_ext::BlockHeight::new(ctx.bitcoind().get_block_count().await as u32),
		hash: ctx.bitcoind().sync_client().get_best_block_hash().unwrap(),
	};
	assert_eq!(srv.bitcoind().sync_client().get_best_block_hash().unwrap(), fork.hash);
	wallet.stop_daemon_wait().await.unwrap();
	drop(wallet);
	srv.start().await.unwrap();
	crate::server_tests::expiry_test::wait_rebuilt(&db, fork, Duration::from_secs(30),
		"server expiry view must observe the actual reorg before Pending").await;
	assert_eq!(db.read(async |tx| tx.get_highest_block(server::database::BlockTable::Captaind).await).await.unwrap(), Some(fork));
	let wallet = super::fallback_adoption::reopen(&ctx, "owner", &srv).await;
	wallet.sync_fallback_payouts().await.unwrap();
	assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
	assert!(wallet.exit_mgr().start_exit_for_vtxos(&[coin.vtxo.clone()]).await.is_err());
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), original.serialize());
}

/// Dust is a policy forfeiture, not an unconfirmed customer payment. Its exact
/// expiry-path sweep must confirm before the wallet stops protecting the coin.
#[tokio::test]
async fn fallback_dust_forfeiture_is_confirmed_and_reorgs_without_server() {
	let ctx = TestContext::new("bark_sdk/fallback_dust_reorg").await;
	let srv = ctx.captaind("server").no_vtxo_pool()
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24))
		.watchmand().create().await;
	// A board must satisfy the real server minimum. The dust-sized coin is
	// created by an ordinary native payment, not a special server setting.
	let source = ctx.bark_sdk("source", &srv).cfg(|cfg| cfg.daemon_manual_sync = true)
		.boarded(sat(20_000)).create().await;
	let wallet = ctx.bark_sdk("owner", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	source.send_arkoor_payment(&wallet.new_address().await.unwrap(), sat(1_200)).await.unwrap();
	wallet.sync().await;
	let coin = wallet.spendable_vtxos().await.unwrap().remove(0);
	assert_eq!(coin.amount(), sat(1_200));
	let signed = wallet.get_full_vtxo(coin.id()).await.unwrap();
	let original = signed.serialize();
	let native_history = wallet.history().await.unwrap();
	let registration = wallet.fallback_script();
	assert!(sat(200) < registration.minimal_non_dust());
	let client_db = SqliteClient::open(ctx.datadir.join("owner/db.sqlite")).unwrap();
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if db.read(async |tx| Ok(tx.query_opt("SELECT id FROM expiry_entitlement WHERE vtxo_id=$1",
				&[&coin.id().to_string()]).await?)).await.unwrap().is_some() { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("native dust allocation was not decided");
	wallet.sync_fallback_payouts().await.unwrap();
	assert!(super::fallback_status_ids(wallet.all_vtxos().await.unwrap(), false).contains(&coin.id()));
	assert!(super::fallback_status_ids(client_db.get_all_vtxos().await.unwrap(), true).is_empty());
	assert_eq!(client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap().state, VtxoState::Locked { holder: None });
	let owed = wallet.balance().await.unwrap();
	assert_eq!(owed.spendable, Amount::ZERO);
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().amount(), sat(1_200));
	assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	assert_eq!(owed.total(), Amount::ZERO);
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let parent = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			wallet.sync_fallback_payouts().await.unwrap();
			for txid in ctx.bitcoind().sync_client().get_raw_mempool().unwrap() {
				let transaction = ctx.bitcoind().await_transaction(txid).await;
				if transaction.input.iter().any(|input| input.previous_output == signed.chain_anchor()) { return transaction; }
			}

			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual dust sweep never broadcast");
	assert!(parent.input.iter().any(|input| input.previous_output == signed.chain_anchor()));
	assert!(!parent.output.iter().any(|output| output.script_pubkey == registration));
	let allocations = db.read(async |tx| Ok(tx.query(
		"SELECT a.sweep_txid,c.disposition,c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat \
		FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id \
		JOIN expiry_allocation a ON a.id=c.allocation_id WHERE e.vtxo_id=$1 AND c.active",
		&[&coin.id().to_string()],
	).await?)).await.unwrap();
	assert_eq!(allocations.len(), 1);
	let allocation = &allocations[0];
	assert_eq!(allocation.get::<_, &str>("sweep_txid"), parent.compute_txid().to_string());
	assert_eq!(allocation.get::<_, &str>("disposition"), "customer");
	assert_eq!((allocation.get::<_, i64>("gross_sat"), allocation.get::<_, i64>("fee_sat"),
		allocation.get::<_, i64>("payout_sat"), allocation.get::<_, i64>("forfeited_sat")),
		(1_200, 0, 0, 1_200));
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	// Raw Core JSON avoids the older RPC crate's unknown P2A script decoder.
	let parent_info: serde_json::Value = ctx.bitcoind().sync_client().call("getrawtransaction",
		&[serde_json::json!(parent.compute_txid()), serde_json::json!(true)]).unwrap();
	assert!(parent_info["confirmations"].as_u64().unwrap() >= 6);
	let confirmed_hash: bitcoin::BlockHash = parent_info["blockhash"].as_str().unwrap().parse().unwrap();
	wallet.chain().invalidate_caches().await;
	wallet.sync_fallback_payouts().await.unwrap();
	let receipts = client_db.get_all_vtxos().await.unwrap();
	assert!(receipts.iter().any(|r| r.id() == coin.id() && r.fallback_status == Some(bark::vtxo::FallbackStatus::Forfeited)));
	assert!(!super::fallback_status_ids(wallet.all_vtxos().await.unwrap(), false).contains(&coin.id()));
	assert_eq!(client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap().state, VtxoState::Spent);
	assert!(!parent.output.iter().any(|output| output.script_pubkey == registration));

	assert_eq!(wallet.balance().await.unwrap().total(), Amount::ZERO,
		"confirmed forfeiture cannot remain counted as another owned Ark claim");
	srv.stop().await.unwrap();
	ctx.bitcoind().sync_client().invalidate_block(&confirmed_hash).unwrap();
	wallet.chain().invalidate_caches().await;
	assert!(wallet.sync_fallback_payouts().await.is_err(), "offline chain data cannot authenticate a changed Ark status");
	wallet.sync_onchain().await.unwrap();
	assert!(ctx.bitcoind().sync_client().get_block_header_info(&confirmed_hash).unwrap().confirmations < 0);
	assert_eq!(client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Forfeited));
	assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
	assert!(wallet.lock_vtxos([coin.id()], None).await.is_err());
	assert!(wallet.exit_mgr().start_exit_for_vtxos(&[coin.vtxo.clone()]).await.is_err());
	assert!(!matches!(wallet.refresh_vtxos([coin.id()]).await, Ok(Some(_))));
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), original);
	assert_eq!(wallet.history().await.unwrap(), native_history, "Bitcoin reorgs do not synthesize Ark receipt history");
	assert_eq!(wallet.balance().await.unwrap().total(), Amount::ZERO);
	srv.bitcoind().sync_client().invalidate_block(&confirmed_hash).unwrap();
	let fork = bitcoin_ext::BlockRef {
		height: bitcoin_ext::BlockHeight::new(ctx.bitcoind().get_block_count().await as u32),
		hash: ctx.bitcoind().sync_client().get_best_block_hash().unwrap(),
	};
	assert_eq!(srv.bitcoind().sync_client().get_best_block_hash().unwrap(), fork.hash);
	wallet.stop_daemon_wait().await.unwrap();
	drop(wallet);
	srv.start().await.unwrap();
	crate::server_tests::expiry_test::wait_rebuilt(&db, fork, Duration::from_secs(30),
		"server expiry view must authenticate the same reorg").await;
	assert_eq!(db.read(async |tx| tx.get_highest_block(server::database::BlockTable::Captaind).await).await.unwrap(), Some(fork));
	let wallet = super::fallback_adoption::reopen(&ctx, "owner", &srv).await;
	wallet.sync_fallback_payouts().await.unwrap();
	assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), original);
	assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
	let reconfirmed_height = ctx.generate_blocks(6).await;
	let point = bitcoin_ext::BlockRef { height: reconfirmed_height,
		hash: ctx.bitcoind().sync_client().get_best_block_hash().unwrap() };
	crate::server_tests::expiry_test::wait_rebuilt(&db, point, Duration::from_secs(30),
		"actual N6 same-parent forfeiture must be rebuilt").await;
	wallet.chain().invalidate_caches().await;
	wallet.sync_fallback_payouts().await.unwrap();
	wallet.sync_onchain().await.unwrap();
	assert_eq!(wallet.onchain().unwrap().read().await.balance().await, Amount::ZERO, "the dust sweep never pays this Bitcoin wallet");
	assert_eq!(client_db.get_wallet_vtxo(coin.id()).await.unwrap().unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Forfeited));
	assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), original);
	assert_eq!(wallet.history().await.unwrap(), native_history);
	assert_eq!(wallet.balance().await.unwrap().total(), Amount::ZERO);
	let reconfirmed: serde_json::Value = ctx.bitcoind().sync_client().call("getrawtransaction",
		&[serde_json::json!(parent.compute_txid()), serde_json::json!(true)]).unwrap();
	assert!(reconfirmed["confirmations"].as_u64().unwrap() >= 6);
}
