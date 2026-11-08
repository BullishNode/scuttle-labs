//! Real server decisions must not discard a restored or imported exit path.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bark::{OpenWalletArgs, Wallet, WalletSeed};
use bark::onchain::OnchainWallet;
use bark::persist::{BarkPersister, sqlite::SqliteClient};
use bark::vtxo::VtxoState;
use bitcoin::{Network, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin::secp256k1::PublicKey;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::Db;
use tokio::sync::{Notify, oneshot};
use ark::vtxo::Full;
use ark::Vtxo;
use server_rpc::protos;

use ark::ProtocolEncoding;
use ark::attestations::VtxoStatusAttestation;
use ark_testing::{require_bitcoind_chain_source, sat, TestContext};
use ark_testing::daemon::captaind;

fn config(config: &mut bark::Config) {
	config.daemon_manual_sync = true;
	config.vtxo_refresh_expiry_threshold = BlockDelta::new(4);
	config.vtxo_exit_margin = BlockDelta::new(2);
}

async fn mnemonic(ctx: &TestContext, name: &str) -> bip39::Mnemonic {
	let text = tokio::fs::read_to_string(ctx.datadir.join(name).join("mnemonic")).await.unwrap();
	bip39::Mnemonic::from_str(text.trim()).unwrap()
}

pub(super) async fn reopen(ctx: &TestContext, name: &str, srv: &captaind::Captaind) -> Wallet {
	// The harness deliberately reallocates listener ports after stop/start.
	// Reopen the existing databases against its current endpoint.
	let mnemonic = mnemonic(ctx, name).await;
	let db = Arc::new(SqliteClient::open(ctx.datadir.join(name).join("db.sqlite")).unwrap());
	let onchain = OnchainWallet::load_or_create(Network::Regtest, mnemonic.to_seed(""), db.clone()).await.unwrap();
	let mut cfg: bark::Config = toml::from_str(&tokio::fs::read_to_string(
		ctx.datadir.join(name).join("config.toml")).await.unwrap()).unwrap();
	cfg.server_address = srv.ark_url();
	Wallet::open(Network::Regtest, WalletSeed::new_from_mnemonic(Network::Regtest, &mnemonic), cfg, OpenWalletArgs {
		persister: Some(db), onchain: Some(Arc::new(tokio::sync::RwLock::new(onchain))), run_daemon: false, ..Default::default()
	}).await.unwrap()
}

#[tokio::test]
async fn full_sync_retires_previously_unseen_paid_mailbox_receipts() {
	require_bitcoind_chain_source!();
	let ctx = TestContext::new("bark_sdk/full_sync_unseen_paid_mailbox").await;
	let srv = ctx.captaind("server").no_vtxo_pool().cfg(|cfg| { cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(24);
		cfg.vtxo_exit_delta = BlockDelta::new(3);
	}).watchmand().create().await;
	let sender = ctx.bark_sdk("sender", &srv).cfg(config).boarded(sat(100_000)).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv).cfg(config).create().await;
	sender.stop_daemon_wait().await.unwrap();
	recipient.stop_daemon_wait().await.unwrap();
	let source = sender.spendable_vtxos().await.unwrap().remove(0);
	let address = recipient.new_address().await.unwrap();
	let destination = recipient.fallback_script();
	let local = SqliteClient::open(ctx.datadir.join("recipient/db.sqlite")).unwrap();
	assert!(recipient.history().await.unwrap().is_empty());
	drop(recipient);
	// The recipient does not sync or reopen its wallet until both deliveries
	// have been paid to its own registered Bitcoin script.
	for _ in 0..2 {
		sender.send_arkoor_payment(&address, sat(10_000)).await.unwrap();
	}
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let rows = db.read(async |tx| tx.get_vtxos_by_policy_page(address.policy(), None, 3).await).await.unwrap();
	assert_eq!(rows.len(), 2);
	let ids = rows.iter().map(|row| row.vtxo_id).collect::<Vec<_>>();
	let coins = db.read(async |tx| tx.get_user_vtxos_by_id(&ids).await).await.unwrap();
	for coin in &coins {
		assert_eq!(coin.vtxo.amount(), sat(10_000));
		assert_eq!(coin.vtxo.chain_anchor(), source.chain_anchor());
		assert_eq!(coin.vtxo.expiry_height(), source.expiry_height());
		assert!(local.get_wallet_vtxo(coin.vtxo_id).await.unwrap().is_none());
	}
	let mut prefixes = coins.iter().flat_map(|coin| coin.vtxo.transactions()
		.map(|step| step.tx.compute_txid())).collect::<BTreeSet<_>>();
	ctx.bitcoind().fund_addr(srv.watchmand().wait_wallet_address().await, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(source.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let blocks_per_prefix = srv.watchmand().config().watchman.progress_grace_period.to_u32() + 1;
	let core = ctx.bitcoind().sync_client();
	let original_ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
	let parents = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let rows = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id,a.sweep_txid,a.sweep_tx \
				FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				JOIN expiry_allocation_state a ON a.id=c.allocation_id \
				WHERE e.vtxo_id=ANY($1) AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='customer' \
				AND a.broadcast_at IS NOT NULL", &[&original_ids]).await?)).await.unwrap();
			assert!(rows.len() <= ids.len());
			if rows.len() == ids.len() {
				assert_eq!(rows.iter().map(|row| row.get::<_, String>("vtxo_id")).collect::<BTreeSet<_>>(),
					original_ids.iter().cloned().collect());
				let parents = rows.iter().map(|row| {
					let parent = deserialize::<Transaction>(row.get("sweep_tx")).unwrap();
					assert_eq!(parent.compute_txid().to_string(), row.get::<_, String>("sweep_txid"));
					(parent.compute_txid(), parent)
				}).collect::<BTreeMap<_, _>>();
				// A database broadcast marker alone does not prove admission.
				if parents.iter().all(|(txid, parent)| core.get_raw_transaction(txid, None).ok().as_ref() == Some(parent)) {
					break parents;
				}
			}
			if let Some(txid) = core.get_raw_mempool().unwrap().into_iter().find(|id| prefixes.contains(id)) {
				assert!(prefixes.remove(&txid));
				let tip = ctx.generate_blocks(blocks_per_prefix).await;
				srv.wait_for_sync_height(tip).await;
				srv.watchmand().wait_for_sync_height(tip).await;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the original recipient deliveries must enter Core's actual payout graph");
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	let mut paid = sat(0);
	for (txid, parent) in &parents {
		for (vout, output) in parent.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == destination) {
			let actual = core.get_tx_out(txid, vout as u32, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations >= 6);
			assert_eq!(actual.value, output.value);
			assert_eq!(actual.script_pub_key.script().unwrap(), destination);
			paid += actual.value;
		}
	}
	assert_eq!(paid, sat(18_000), "the two deliveries share one cumulative 2,000-sat fee");
	for id in &ids { assert!(local.get_wallet_vtxo(*id).await.unwrap().is_none()); }
	let returned = reopen(&ctx, "recipient", &srv).await;
	assert!(returned.history().await.unwrap().is_empty());
	assert_eq!(returned.properties().await.unwrap().server_payouts, Some(true));
	let err = returned.import_vtxo(&coins[0].vtxo, bark::ImportVtxoArgs {
		skip_status_check: true, ..Default::default()
	}).await.unwrap_err();
	assert!(err.to_string().contains("require authenticated mailbox ownership and status"));
	for id in &ids { assert!(local.get_wallet_vtxo(*id).await.unwrap().is_none()); }
	returned.sync().await;
	for coin in &coins {
		assert_eq!(local.get_wallet_vtxo(coin.vtxo_id).await.unwrap().unwrap().state, VtxoState::Spent);
		assert_eq!(returned.get_full_vtxo(coin.vtxo_id).await.unwrap().serialize(), coin.vtxo.serialize());
	}
	assert!(returned.spendable_vtxos().await.unwrap().is_empty());
	assert_eq!(returned.balance().await.unwrap().total(), sat(0));
	let history = returned.history().await.unwrap();
	assert!(history.is_empty(), "already paid deliveries do not credit an Ark receive");
	returned.sync_onchain().await.unwrap();
	// The canonical output total18k and cumulative fee2k were proved above.
	assert_eq!(paid + sat(2_000), sat(20_000));
	for _ in 0..2 {
		returned.sync().await;
		assert_eq!(returned.history().await.unwrap(), history);
		assert_eq!(returned.balance().await.unwrap().total(), sat(0));
	}

}

#[derive(Clone, Default)]
struct HoldArkoorRegistration {
	arrived: Arc<Mutex<Option<(PublicKey, oneshot::Sender<protos::RegisterVtxoTransactionsRequest>)>>>,
	release: Arc<Notify>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for HoldArkoorRegistration {
	async fn register_vtxo_transactions(
		&self, upstream: &mut captaind::ArkClient, request: protos::RegisterVtxoTransactionsRequest,
	) -> Result<protos::Empty, tonic::Status> {
		// Input-chain registration precedes cosigning. Only the new recipient's
		// signed outputs reach the durable Registration checkpoint under test.
		let arrived = {
			let mut gate = self.arrived.lock().unwrap();
			if gate.as_ref().is_some_and(|(key, _)| request.vtxos.iter().any(|raw|
				Vtxo::<Full>::deserialize(raw).is_ok_and(|coin| coin.user_pubkey() == *key))) {
				gate.take().map(|(_, arrived)| arrived)
			} else { None }
		};
		if let Some(arrived) = arrived {
			arrived.send(request.clone()).unwrap();
			self.release.notified().await;
		}
		Ok(upstream.register_vtxo_transactions(request).await?.into_inner())
	}

}

#[tokio::test]
async fn provisional_recipient_transfer_pays_input_owner() {
	Box::pin(stalled_change()).await;
}

async fn stalled_change() {
	let ctx = TestContext::new("bark_sdk/stalled_arkoor_change_pending").await;
	let srv = ctx.captaind("server").no_vtxo_pool().cfg(|config| { config.expiry_payouts = true;
		config.vtxo_lifetime = BlockDelta::new(24);
		config.vtxo_exit_delta = BlockDelta::new(3);
	}).watchmand().create().await;
	let gate = HoldArkoorRegistration::default();
	let proxy = captaind::proxy::ArkRpcProxyServer::start(
		(gate.clone(), srv.get_public_rpc().await), ((), srv.get_mailbox_public_rpc().await),
	).await;
	let owner = Arc::new(ctx.bark_sdk("owner", &proxy.address).cfg(config)
		.funded(sat(500_000)).boarded(sat(100_000)).create().await);
	let recipient = ctx.bark_sdk("recipient", &srv).cfg(config).create().await;
	let address = recipient.new_address().await.unwrap();
	let recipient_key = address.policy().user_pubkey();
	let source = owner.spendable_vtxos().await.unwrap().remove(0);
	let source_key = source.user_pubkey();
	let destination = owner.fallback_script();
	let (arrived, prepared) = oneshot::channel();
	*gate.arrived.lock().unwrap() = Some((recipient_key, arrived));
	let sender = owner.clone();
	let payment = tokio::spawn(async move { sender.send_arkoor_payment(&address, sat(60_000)).await });
	let request = tokio::time::timeout(Duration::from_secs(15), prepared).await.unwrap().unwrap();
	let leaves = request.vtxos.iter().map(|raw| Vtxo::<Full>::deserialize(raw).unwrap()).collect::<Vec<_>>();
	let changes = leaves.iter().filter(|coin| coin.user_pubkey() != recipient_key).cloned().collect::<Vec<_>>();
	let received = leaves.iter().find(|coin| coin.user_pubkey() == recipient_key).unwrap();
	assert!(!changes.is_empty());
	assert!(changes.iter().all(|coin| coin.user_pubkey() != source_key));
	assert_eq!(changes.iter().map(|coin| coin.amount()).sum::<bitcoin::Amount>(), sat(40_000));
	assert_eq!(received.amount(), sat(60_000));
	let local = SqliteClient::open(ctx.datadir.join("owner/db.sqlite")).unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let ids = leaves.iter().map(|coin| coin.id().to_string()).collect::<Vec<_>>();
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(source.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let decided = db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_entitlement \
				WHERE vtxo_id=ANY($1)", &[&ids]).await?.get::<_, i64>(0))).await.unwrap();
			if usize::try_from(decided).unwrap() == leaves.len() { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("real unregistered outputs must be allocated before registration resumes");

	gate.release.notify_one();
	let error = tokio::time::timeout(Duration::from_secs(15), payment)
		.await.unwrap().unwrap().unwrap_err();
	assert_eq!(bark::FallbackRefusal::from_error(&error), Some(bark::FallbackRefusal::DecidedForFallback));
	owner.sync().await;
	let own_status = protos::VtxoSpendState::FallbackPending;
	let sends = owner.history().await.unwrap().into_iter().filter(|movement| movement.subsystem.name == "bark.arkoor")
		.collect::<Vec<_>>();
	assert_eq!(sends.len(), 1);
	assert_eq!(sends[0].status, bark::movement::MovementStatus::Failed);
	assert_eq!(sends[0].intended_balance, bitcoin::SignedAmount::from_sat(-60_000));
	assert_eq!(sends[0].effective_balance, bitcoin::SignedAmount::ZERO,
		"the input-owner commitment did not transfer 60k to the provisional recipient");
	let mut rpc = srv.get_public_rpc().await;
	let original_status = rpc.get_vtxo_status(protos::GetVtxoStatusRequest {
		vtxo_id: source.id().to_bytes().to_vec(),
		attestation: VtxoStatusAttestation::new(source.id(), &owner.get_vtxo_key(&source).await.unwrap()).serialize(),
	}).await.unwrap().into_inner();
	assert_eq!(original_status.spend_state, own_status as i32,
		"all provisional split portions project back to the known input owner");
	for coin in &leaves {
		let key = if coin.user_pubkey() == recipient_key { recipient.get_vtxo_key(coin).await.unwrap() }
			else { owner.get_vtxo_key(coin).await.unwrap() };
		let state = rpc.get_vtxo_status(protos::GetVtxoStatusRequest {
			vtxo_id: coin.id().to_bytes().to_vec(),
			attestation: VtxoStatusAttestation::new(coin.id(), &key).serialize(),
		}).await.unwrap().into_inner().spend_state;
		assert_eq!(state, if coin.user_pubkey() == recipient_key { protos::VtxoSpendState::Spent }
			else { own_status } as i32);
	}
	assert_eq!(owner.balance().await.unwrap().total(), sat(0), "known committed inputs are status-only, not new Ark balance");
	assert!(owner.spendable_vtxos().await.unwrap().is_empty());
	assert_eq!(owner.get_vtxo_by_id(source.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	assert_eq!(owner.get_full_vtxo(source.id()).await.unwrap().amount(), source.amount());
	for change in &changes {
		assert_eq!(owner.get_vtxo_by_id(change.id()).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	}
	recipient.sync_fallback_payouts().await.unwrap();
	assert!(recipient.all_vtxos().await.unwrap().iter().all(|coin| coin.fallback_status.is_none()));
	assert_eq!(recipient.balance().await.unwrap().total(), sat(0));
	ctx.bitcoind().fund_addr(srv.watchmand().wait_wallet_address().await, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let payout = tokio::time::timeout(Duration::from_secs(30), async {
		'payout: loop {
			for txid in ctx.bitcoind().sync_client().get_raw_mempool().unwrap() {
				let tx = ctx.bitcoind().await_transaction(txid).await;
				if tx.output.iter().any(|output| output.script_pubkey == destination && output.value == sat(98_000)) { break 'payout tx; }
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the retained input owner's real payout must become relayable after funding");
	ctx.generate_blocks(6).await;
	owner.chain().invalidate_caches().await;
	owner.sync_fallback_payouts().await.unwrap();
	owner.sync_pending_arkoor_sends().await.unwrap();
	assert!(owner.pending_arkoor_sends().await.unwrap().is_empty(),
		"actual canonical retirement permits removal of the protected checkpoint");
	for change in &changes {
		assert_eq!(local.get_wallet_vtxo(change.id()).await.unwrap().unwrap().state, VtxoState::Spent);
	}
	assert_eq!(owner.balance().await.unwrap().total(), sat(0));
	let history = owner.history().await.unwrap();
	assert_eq!(history.iter().map(|movement| movement.effective_balance.to_sat()).sum::<i64>(), 100_000,
		"ordinary board history stays intact without a synthetic BTC payout debit");
	let sends = history.iter().filter(|movement| movement.subsystem.name == "bark.arkoor").collect::<Vec<_>>();
	assert_eq!(sends.len(), 1);
	assert_eq!(sends[0].status, bark::movement::MovementStatus::Failed);
	assert_eq!(sends[0].effective_balance, bitcoin::SignedAmount::ZERO);
	let history = serde_json::to_value(history).unwrap();
	owner.sync_pending_arkoor_sends().await.unwrap();
	owner.sync_fallback_payouts().await.unwrap();
	assert_eq!(serde_json::to_value(owner.history().await.unwrap()).unwrap(), history);
	let (index, _) = payout.output.iter().enumerate().find(|(_, output)| output.script_pubkey == destination && output.value == sat(98_000)).unwrap();
	let output = ctx.bitcoind().sync_client().get_tx_out(&payout.compute_txid(), index as u32, Some(true)).unwrap().unwrap();
	assert!(output.confirmations >= 6);
	assert_eq!(output.value, sat(98_000));
	assert_eq!(output.script_pub_key.script().unwrap(), destination);

}
