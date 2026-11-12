//! Released daemon payout history and seed restoration.

use std::time::Duration;

use bitcoin::{Address, Amount, Network, OutPoint};
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::RpcApi;
use server::database::Db;

use ark_testing::{sat, TestContext};
use ark_testing::constants::BOARD_CONFIRMATIONS;

#[tokio::test]
async fn fallback_payout_history_and_seed_restore() {
	let ctx = TestContext::new("barkd/fallback_payout_history_and_seed_restore").await;
	let srv = ctx.captaind("server").no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand().create().await;
	let mnemonic = bip39::Mnemonic::from_entropy(&[43; 32]).unwrap().to_string();
	let barkd = ctx.barkd("wallet", &srv).mnemonic(mnemonic.clone())
		.cfg(|cfg| cfg.daemon_manual_sync = true).funded(sat(200_000)).create().await;
	let config = barkd.client_config();
	let identity: bark_json::cli::DebugInfo = config.client
		.get(format!("{}/api/v1/wallet/debug-info", config.base_path)).send().await.unwrap()
		.error_for_status().unwrap().json().await.unwrap();
	assert_eq!(identity.network, Network::Regtest);
	let script = ark::fallback::payout_script(identity.mailbox_id);
	let address = Address::from_script(&script, identity.network).unwrap();
	assert!(script.is_p2tr(), "the released daemon must use its native BIP86 wallet");
	assert_ne!(address, barkd.onchain_address().await);
	barkd.onchain_sync().await;
	let board = barkd.board_amount(sat(100_000)).await;
	ctx.await_transaction(board.funding_tx.txid).await;
	let admitted_height = ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	srv.wait_for_sync_height(admitted_height).await;
	barkd.sync().await;
	let coins = barkd.vtxos(None).await;
	assert_eq!(coins.len(), 1);
	assert_eq!(coins[0].state, bark_json::primitives::VtxoStateInfo::Spendable);
	let coin = &coins[0];
	let expected = coin.vtxo.amount - sat(2_000);
	let original = barkd.get_vtxo_encoded(&coin.vtxo.id.to_string()).await.encoded;

	// Absence means the native process is stopped, including every
	// maintenance and sync task. No wallet observer opens it during expiry.
	barkd.stop().await.unwrap();
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(coin.vtxo.expiry_height.to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let (parent, index) = tokio::time::timeout(Duration::from_secs(60), async {
		loop {
			for txid in ctx.bitcoind().sync_client().get_raw_mempool().unwrap() {
				let tx = ctx.bitcoind().await_transaction(txid).await;
				if let Some(index) = tx.output.iter().position(|output|
					output.script_pubkey == script && output.value == expected) {
					return (tx, index as u32);
				}
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual background payout was not broadcast");
	let point = OutPoint::new(parent.compute_txid(), index);
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	assert_eq!(ctx.bitcoind().sync_client().get_tx_out(&point.txid, point.vout, Some(true))
		.unwrap().unwrap().value, expected);
	barkd.start().await.unwrap();
	// The returning wallet asks for the status of its expired coin.
	barkd.sync().await;
	let paid_coin = barkd.get_vtxo(&coin.vtxo.id.to_string()).await;
	assert_eq!(paid_coin.state, bark_json::primitives::VtxoStateInfo::Spent);
	assert_eq!(paid_coin.fallback_status, Some(bark_json::primitives::FallbackStatusInfo::Spent));
	assert_eq!(barkd.get_vtxo_encoded(&coin.vtxo.id.to_string()).await.encoded, original);
	let postgres = srv.config().postgres.clone();
	let db = Db::connect(&postgres).await.unwrap();
	let native_history = barkd.history(None, None).await;
	// Bark reports the server's fallback status, while Core independently
	// proves the Bitcoin output. Bark's ordinary on-chain wallet does not watch
	// the separate tr(M) derivation.
	barkd.sync().await;
	barkd.onchain_sync().await;
	assert_eq!(barkd.get_vtxo(&coin.vtxo.id.to_string()).await.state,
		bark_json::primitives::VtxoStateInfo::Spent);
	assert_eq!(barkd.history(None, None).await, native_history, "chain discovery must not invent a native action");
	assert_eq!(barkd.get_vtxo_encoded(&coin.vtxo.id.to_string()).await.encoded, original);
	assert_eq!(barkd.bark_balance().await.spendable, Amount::ZERO);
	assert!(barkd.onchain_utxos().await.iter().all(|utxo| utxo.outpoint != point));
	let allocations = db.read(async |tx| Ok(tx.query(
		"SELECT a.sweep_txid,c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat \
		FROM expiry_allocation_coin c \
		JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE c.vtxo_id=$1 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')",
		&[&coin.vtxo.id.to_string()],
	).await?)).await.unwrap();
	assert_eq!(allocations.len(), 1);
	let allocation = &allocations[0];
	assert_eq!(allocation.get::<_, &str>("sweep_txid"), point.txid.to_string());
	assert_eq!((allocation.get::<_, i64>("gross_sat"), allocation.get::<_, i64>("fee_sat"),
		allocation.get::<_, i64>("payout_sat"), allocation.get::<_, i64>("forfeited_sat")),
		(100_000, 2_000, 98_000, 0));
	assert_eq!(coin.vtxo.amount, expected + sat(2_000));

	barkd.stop().await.unwrap();

	let restored = ctx.barkd("seed_restored", &srv).mnemonic(mnemonic)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	restored.sync().await;
	restored.onchain_sync().await;
	let restored_config = restored.client_config();
	let restored_identity: bark_json::cli::DebugInfo = restored_config.client
		.get(format!("{}/api/v1/wallet/debug-info", restored_config.base_path)).send().await.unwrap()
		.error_for_status().unwrap().json().await.unwrap();
	assert_eq!(restored_identity.network, identity.network);
	assert_eq!(restored_identity.mailbox_id, identity.mailbox_id);
	assert_eq!(ark::fallback::payout_script(restored_identity.mailbox_id), script);
	let restored_coin = restored.vtxos(Some(true)).await.into_iter()
		.find(|vtxo| vtxo.vtxo.id == coin.vtxo.id).expect("seed restore must retain the payout source");
	assert_eq!(restored_coin.state, bark_json::primitives::VtxoStateInfo::Spent);
	assert_eq!(restored_coin.fallback_status, Some(bark_json::primitives::FallbackStatusInfo::Spent));
	assert!(restored.onchain_utxos().await.iter().all(|utxo| utxo.outpoint != point));
	restored.stop().await.unwrap();
	srv.watchmand().stop().await.unwrap();
	srv.stop().await.unwrap();
}
