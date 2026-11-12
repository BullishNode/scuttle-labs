use std::time::Duration;
use bitcoincore_rpc::RpcApi;
use bitcoin::Amount;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::BitcoinRpcExt;
use server::database::Db;
use ark_testing::{TestContext, btc, sat};
use ark_testing::constants::ROUND_CONFIRMATIONS;
use bitcoin::{Network, OutPoint, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin_ext::TxOutExt;

/// Two different owners in the same refreshed round root receive distinct
/// direct outputs. The old boards are spent and cannot be paid a second time.
#[tokio::test]
pub(super) async fn round_root_pays_both_current_owners_once() {
	let ctx = TestContext::new("server/payouts/round_root_pays_both_current_owners_once").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(48);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let alice = ctx.bark("alice", &srv).funded(sat(300_000)).create().await;
	let bob = ctx.bark("bob", &srv).funded(sat(300_000)).create().await;
	alice.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	bob.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	ctx.refresh_all(&srv, &[&alice, &bob]).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	alice.sync().await;
	bob.sync().await;
	let mut owners = Vec::new();
	for bark in [&alice, &bob] {
		let client = bark.client().await;
		let coins = client.vtxos().await.unwrap();
		assert_eq!(coins.len(), 1);
		let coin = client.get_full_vtxo(coins[0].id()).await.unwrap();
		assert_eq!(coin.amount(), sat(200_000));
		owners.push((coin, client.fallback_script()));
	}
	assert_eq!(owners[0].0.chain_anchor(), owners[1].0.chain_anchor());
	assert_ne!(owners[0].1, owners[1].1);
	let root = owners[0].0.chain_anchor();
	let expiry = owners[0].0.expiry_height().to_u32();
	assert_eq!(expiry, owners[1].0.expiry_height().to_u32());
	let wm = srv.watchmand();
	ctx.bitcoind().fund_addr(wm.wait_wallet_address().await, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let current = ctx.bitcoind().get_block_count().await as u32;
	assert!(current < expiry);
	let tip = ctx.generate_blocks(expiry + 1 - current).await;
	srv.wait_for_sync_height(tip).await;
	wm.wait_for_sync_height(tip).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let source_ids = owners.iter().map(|(coin, _)| coin.id().to_string()).collect::<Vec<_>>();
	let (allocation, parent) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let rows = db.read(async |tx| Ok(tx.query(
				"SELECT a.id,a.sweep_tx FROM expiry_allocation_state a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				WHERE c.vtxo_id=ANY($1) AND c.disposition='customer' \
				AND a.status<>'conflicted' AND a.broadcast_at IS NOT NULL",
				&[&source_ids],
			).await?)).await.unwrap();
			if rows.len() == 2 {
				assert_eq!(rows[0].get::<_, i64>("id"), rows[1].get::<_, i64>("id"));
				break (rows[0].get::<_, i64>("id"),
					deserialize::<Transaction>(rows[0].get("sweep_tx")).unwrap());
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("both current owners must share one published direct payout");
	assert_eq!(parent.version.0, 2);
	assert_eq!(parent.input.iter().filter(|input| input.previous_output == root).count(), 1);
	assert!(parent.output.iter().all(|out| !out.is_p2a_fee_anchor()));
	let operator_script = wm.config().sweep_address.clone().unwrap()
		.require_network(Network::Regtest).unwrap().script_pubkey();
	for (_, destination) in &owners {
		let outputs = parent.output.iter().filter(|out| out.script_pubkey == *destination)
			.collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert_eq!(outputs[0].value, sat(198_000));
	}
	assert!(parent.output.iter().all(|out| owners.iter().any(|(_, script)| *script == out.script_pubkey)
		|| out.script_pubkey == operator_script));
	let rows = db.read(async |tx| Ok(tx.query(
		"SELECT c.vtxo_id,c.disposition,c.gross_sat,c.fee_sat,c.payout_sat \
		FROM expiry_allocation_coin c \
		WHERE c.allocation_id=$1", &[&allocation],
	).await?)).await.unwrap();
	assert_eq!(rows.len(), 2);
	for row in rows {
		assert!(source_ids.contains(&row.get::<_, String>("vtxo_id")));
		assert_eq!(row.get::<_, &str>("disposition"), "customer");
		assert_eq!(row.get::<_, i64>("gross_sat"), 200_000);
		assert_eq!(row.get::<_, i64>("fee_sat"), 2_000);
		assert_eq!(row.get::<_, i64>("payout_sat"), 198_000);
	}
	let fee = watchman_fee(&ctx, &parent);
	assert!(fee > Amount::ZERO);
	ctx.generate_blocks(6).await;
	assert_canonical_watchman_tx(&ctx, &parent);
	let core = ctx.bitcoind().sync_client();
	for (_, destination) in &owners {
		let (index, output) = parent.output.iter().enumerate()
			.find(|(_, out)| out.script_pubkey == *destination).unwrap();
		let actual = core.get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 6);
		assert_eq!(actual.value, output.value);
	}
}

#[tokio::test]
pub(super) async fn watchman_sweeps_offboard_connectors() {
	let ctx = TestContext::new("server/payouts/watchman_sweeps_offboard_connectors").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(24);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let wm = srv.watchmand();
	let fee_address = wm.wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(20_000)).await;
	ctx.generate_blocks(2).await;

	let bark = ctx.bark_sdk("bark1", &srv).funded(sat(500_000))
		.boarded(sat(100_000)).boarded(sat(100_000)).create().await;
	bark.sync().await;
	let board = bark.vtxos().await.unwrap();
	assert_eq!(board.len(), 2);
	let payout = ctx.bitcoind().get_new_address();
	let offboard_txid = bark.offboard_all(payout).await.unwrap().txid;
	ctx.generate_blocks(1).await;
	let offboard = ctx.bitcoind().await_transaction(offboard_txid).await;
	let connector = OutPoint::new(offboard_txid, 1);
	assert_eq!(offboard.output[1].value, sat(660));
	let connector_id = ark::VtxoId::from(connector);
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let expiry = db.read(async |tx| Ok(tx.query_one(
		"SELECT expiry FROM vtxo WHERE vtxo_id=$1", &[&connector_id.to_string()],
	).await?.get::<_, i32>(0))).await.unwrap();
	let expiry = u32::try_from(expiry).unwrap();

	let current = ctx.bitcoind().get_block_count().await as u32;
	assert!(current < expiry - 1);
	let early = ctx.generate_blocks(expiry - 1 - current).await;
	srv.wait_for_sync_height(early).await;
	wm.wait_for_sync_height(early).await;
	wm.trigger_sweep().await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&connector.txid, connector.vout, Some(true))
		.unwrap().is_some(), "connector must remain until its own expiry");
	let early_allocations = db.read(async |tx| Ok(tx.query_one(
		"SELECT count(*) FROM expiry_allocation_input WHERE vtxo_id=$1",
		&[&connector_id.to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(early_allocations, 0);

	let expired = ctx.generate_blocks(2).await;
	srv.wait_for_sync_height(expired).await;
	wm.wait_for_sync_height(expired).await;
	wm.trigger_sweep().await;
	let (allocation, parent) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |tx| Ok(tx.query_opt(
				"SELECT a.id,a.sweep_tx FROM expiry_allocation_state a \
				JOIN expiry_allocation_input i ON i.allocation_id=a.id \
				WHERE i.vtxo_id=$1 AND a.status<>'conflicted' AND a.broadcast_at IS NOT NULL",
				&[&connector_id.to_string()],
			).await?)).await.unwrap();
			if let Some(row) = row { break (row.get::<_, i64>("id"), deserialize::<Transaction>(row.get("sweep_tx")).unwrap()); }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("tiny operator connector needs a funded direct expiry sweep");
	assert_eq!(parent.version.0, 2);
	assert!(parent.input.iter().any(|input| input.previous_output == connector));
	assert!(parent.input.len() >= 2, "the connector needs a sponsor for a standard drain output");
	let operator_script = wm.config().sweep_address.clone().unwrap()
		.require_network(Network::Regtest).unwrap().script_pubkey();
	assert_eq!(parent.output.len(), 1);
	assert_eq!(parent.output[0].script_pubkey, operator_script);
	assert!(parent.output[0].value >= parent.output[0].script_pubkey.minimal_non_dust());
	let rows = db.read(async |tx| Ok(tx.query(
		"SELECT disposition,gross_sat,payout_sat FROM expiry_allocation_coin WHERE allocation_id=$1",
		&[&allocation],
	).await?)).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].get::<_, &str>("disposition"), "operator");
	assert_eq!(rows[0].get::<_, i64>("gross_sat"), 660);
	assert_eq!(rows[0].get::<_, i64>("payout_sat"), 0);
	let fee = watchman_fee(&ctx, &parent);
	assert!(fee > Amount::ZERO);
	let recorded_fee = db.read(async |tx| Ok(tx.query_one(
		"SELECT fee_sat FROM expiry_payout_attempt WHERE allocation_id=$1 AND txid=$2",
		&[&allocation, &parent.compute_txid().to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(fee.to_sat(), u64::try_from(recorded_fee).unwrap());
	ctx.generate_blocks(6).await;
	assert_canonical_watchman_tx(&ctx, &parent);
	assert!(ctx.bitcoind().sync_client().get_tx_out(&connector.txid, connector.vout, Some(true))
		.unwrap().is_none());
}


fn watchman_fee(ctx: &TestContext, tx: &Transaction) -> Amount {
	let core = ctx.bitcoind().sync_client();
	let inputs = tx.input.iter().map(|input| {
		let previous = core.get_raw_transaction(&input.previous_output.txid, None).unwrap();
		previous.output[input.previous_output.vout as usize].value
	}).sum::<Amount>();
	inputs.checked_sub(tx.output.iter().map(|output| output.value).sum::<Amount>()).unwrap()
}


pub(super) fn assert_canonical_watchman_tx(ctx: &TestContext, tx: &Transaction) {
	let core = ctx.bitcoind().sync_client();
	let info = core.custom_get_raw_transaction_info(tx.compute_txid(), None).unwrap().unwrap();
	assert!(info.confirmations.unwrap_or(0) >= 6);
	let hash = info.blockhash.unwrap();
	let header = core.get_block_header_info(&hash).unwrap();
	assert_eq!(core.get_block_hash(header.height as u64).unwrap(), hash);
	assert!(core.get_block(&hash).unwrap().txdata.iter().any(|actual| actual == tx));
}
