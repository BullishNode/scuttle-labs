use std::time::Duration;

use bitcoin::consensus::encode::serialize_hex;
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use serde_json::json;

use ark::VtxoId;
use server::database::Db;

use ark_testing::{sat, Bitcoind, TestContext};

use super::expiry_operation_races::wait_for_query_lock;

/// A reorg lands after a payout decision analysed an expired root and before
/// it commits. The replacement branch confirms the owner's own exit of that
/// root. The stale analysis must not commit, and no payout is ever decided
/// for the exited coin.
///
/// The analysis is held after it took its snapshot by an exclusive lock on a
/// table it reads last. The reorg is real: a side node mines a longer branch
/// from the block before expiry, and its blocks are submitted to Core, which
/// switches to it in one step, so neither daemon sees a lower tip.
#[tokio::test]
async fn expiry_reorg_between_plan_and_commit_aborts_the_stale_plan() {
	let ctx = TestContext::new("server/expiry_plan_commit_reorg").await;
	let core_url = ctx.bitcoind().rpc_url();
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(move |cfg| cfg.bitcoind.url = core_url).create().await;
	let wallet = ctx.bark("owner", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	assert_eq!(ids.len(), 1);
	let client = wallet.client().await;
	let coin = client.get_full_vtxo(ids[0]).await.unwrap();
	drop(client);
	let root = VtxoId::from(coin.chain_anchor());
	let exit = coin.transactions().map(|step| step.tx).collect::<Vec<_>>();
	let exit_txid = exit.iter().find(|tx| tx.input.iter().any(|i| i.previous_output == coin.chain_anchor()))
		.expect("the exit spends the board funding output").compute_txid();
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let expiry = coin.expiry_height().to_u32();
	let before = ctx.bitcoind().get_block_count().await as u32;
	assert!(before + 1 < expiry);
	let fork = ctx.generate_blocks(expiry - 1 - before).await;
	srv.watchmand().wait_for_sync_height(fork).await;

	// The side node starts from the test's snapshot and copies the chain up
	// to the block before expiry. It has no peer, so it stays there.
	let chain = ctx.bitcoind().sync_client();
	let side = Bitcoind::new("side".into(), ctx.bitcoind_default_cfg("side"), None);
	side.start().await.unwrap();
	let side_chain = side.sync_client();
	for height in side_chain.get_block_count().unwrap() + 1..=fork.to_u32() as u64 {
		let hash = chain.get_block_hash(height).unwrap();
		let raw = chain.call::<String>("getblock", &[json!(hash), json!(0)]).unwrap();
		side_chain.call::<serde_json::Value>("submitblock", &[json!(raw)]).unwrap();
	}
	assert_eq!(side_chain.get_best_block_hash().unwrap(), chain.get_best_block_hash().unwrap());

	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let old_hash = db.write(async |tx| {
		// Held until this transaction ends. Only the decision's branch
		// analysis reads this table here; it already holds its snapshot.
		tx.batch_execute("LOCK TABLE lightning_payment_attempt_htlc_vtxo IN ACCESS EXCLUSIVE MODE").await?;
		let mature = ctx.generate_blocks(1).await;
		assert_eq!(mature, BlockHeight::new(expiry));
		let old_hash = chain.get_block_hash(expiry as u64)?;
		wait_for_query_lock(tx, "SELECT DISTINCT a.id,a.lightning_htlc_subscription_id%").await?;

		// The replacement branch: the owner's exit at the expiry height,
		// then one more block, so Core switches to it.
		let miner = ctx.bitcoind().get_new_address().to_string();
		let exit_hex = exit.iter().map(|tx| serialize_hex(tx)).collect::<Vec<_>>();
		side_chain.call::<serde_json::Value>("generateblock", &[json!(miner), json!(exit_hex)])?;
		side_chain.call::<serde_json::Value>("generateblock", &[json!(miner), json!([])])?;
		for height in expiry as u64..=expiry as u64 + 1 {
			let raw = side_chain.call::<String>("getblock", &[json!(side_chain.get_block_hash(height)?), json!(0)])?;
			chain.call::<serde_json::Value>("submitblock", &[json!(raw)])?;
		}
		assert_eq!(chain.get_best_block_hash()?, side_chain.get_best_block_hash()?);
		assert_ne!(chain.get_block_hash(expiry as u64)?, old_hash);

		// Release the analysis only once watchmand removed the orphaned block.
		tokio::time::timeout(Duration::from_secs(60), async {
			loop {
				let stored = tx.query_opt("SELECT hash FROM watchmand_block WHERE height=$1",
					&[&i64::from(expiry)]).await?.map(|row| row.get::<_, String>("hash"));
				if stored != Some(old_hash.to_string()) { return Ok::<_, anyhow::Error>(()); }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("watchmand never removed the orphaned block")?;
		assert_eq!(tx.query_one("SELECT COUNT(*) FROM expiry_allocation", &[]).await?.get::<_, i64>(0), 0);
		Ok(old_hash)
	}).await.unwrap();

	// The stale plan reaches its commit and aborts on the moved tip.
	let log = srv.watchmand().config().data_dir.join("stdout.log");
	tokio::time::timeout(Duration::from_secs(60), async {
		while !std::fs::read_to_string(&log).unwrap().contains("published watchman tip changed") {
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}).await.expect("the stale plan never reached its tip check");

	// Later decision passes see the exit on the canonical chain.
	let tip = ctx.generate_blocks(2).await;
	srv.watchmand().wait_for_sync_height(tip).await;
	tokio::time::sleep(Duration::from_secs(3)).await;
	assert_ne!(chain.get_block_hash(expiry as u64).unwrap(), old_hash);
	let (spent_by, allocations, entitlements) = db.read(async |t| {
		let spent_by = t.query_one("SELECT onchain_spent_txid FROM vtxo WHERE vtxo_id=$1",
			&[&root.to_string()]).await?.get::<_, Option<String>>(0);
		let allocations = t.query_one("SELECT COUNT(*) FROM expiry_allocation", &[]).await?.get::<_, i64>(0);
		let entitlements = t.query_one("SELECT COUNT(*) FROM expiry_entitlement", &[]).await?.get::<_, i64>(0);
		Ok((spent_by, allocations, entitlements))
	}).await.unwrap();
	assert_eq!(spent_by, Some(exit_txid.to_string()), "the root is spent by the owner's exit");
	assert_eq!((allocations, entitlements), (0, 0), "no payout is decided for an exited coin");
}
