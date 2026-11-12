//! Real server decisions must not discard a restored or imported exit path.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bark::{OpenWalletArgs, Wallet, WalletSeed};
use bark::onchain::OnchainWallet;
use bark::persist::{BarkPersister, sqlite::SqliteClient};
use bark::vtxo::VtxoState;
use bitcoin::{Network, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::RpcApi;
use server::database::Db;

use ark::ProtocolEncoding;
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
			let rows = db.read(async |tx| Ok(tx.query("SELECT c.vtxo_id,a.sweep_txid,a.sweep_tx \
				FROM expiry_allocation_coin c \
				JOIN expiry_allocation_state a ON a.id=c.allocation_id \
				WHERE c.vtxo_id=ANY($1) AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='customer' \
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
	returned.sync().await;
	for coin in &coins {
		assert_eq!(local.get_wallet_vtxo(coin.vtxo_id).await.unwrap().unwrap().state, VtxoState::Spent);
		assert_eq!(returned.get_full_vtxo(coin.vtxo_id).await.unwrap().serialize(), coin.vtxo.serialize());
		assert_eq!(returned.get_vtxo_by_id(coin.vtxo_id).await.unwrap().fallback_status,
			Some(bark::vtxo::FallbackStatus::Spent));
		let payout = returned.fallback_payout(coin.vtxo_id).await.unwrap().unwrap();
		assert_eq!(payout.gross, sat(10_000));
		assert_eq!(payout.address.script_pubkey(), destination);
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

