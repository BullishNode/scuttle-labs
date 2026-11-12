use super::expiry_test::{actual_fee, wait_payout as wait_for_payout};

use std::time::Duration;

use ark_testing::{sat, TestContext};
use bitcoin::{BlockHash, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin_ext::{BlockDelta, BlockRef, TxOutExt};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::{BlockTable, Db};

async fn wait_for_confirmation(db: &Db, allocation: i64, point: BlockRef, hash: BlockHash) {
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let ready = db.read(async |t| {
				let row = t.query_one("SELECT status,confirmed_hash FROM expiry_allocation_state WHERE id=$1", &[&allocation]).await?;
				Ok(t.get_highest_block(BlockTable::Captaind).await? == Some(point)
					&& t.get_highest_block(BlockTable::Watchmand).await? == Some(point)
					&& row.get::<_, &str>("status") == "confirmed"
					&& row.get::<_, Option<&str>>("confirmed_hash") == Some(hash.to_string().as_str()))
			}).await.unwrap();
			if ready { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("canonical payout confirmation was not rebuilt after actual daemon sync")
}

/// No fixture inserts entitlements, allocations, chain acknowledgments or
/// invalidation flags. Real Core forks and daemon restart drive the rebuild.
#[tokio::test]
async fn expiry_actual_payout_reorg_preserves_receipt_and_fence() {
	let ctx = TestContext::new("server/expiry_payout_reorg_6").await;
	// Both daemons observe the same real Core fork.
	let core_url = ctx.bitcoind().rpc_url();
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(move |cfg| cfg.bitcoind.url = core_url).create().await;
	let wallet = ctx.bark("abandoned", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	assert_eq!(ids.len(), 1);
	let client = wallet.client().await;
	let coin = client.get_full_vtxo(ids[0]).await.unwrap();
	let destination = client.fallback_script();
	let expected = coin.amount() - sat(2_000);
	drop(client);
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let before = ctx.bitcoind().get_block_count().await as u32;
	assert!(before < coin.expiry_height().to_u32());
	ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - before).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (allocation, parent) = wait_for_payout(&db, ids[0]).await;
	let txid = parent.compute_txid();
	assert_eq!(ctx.bitcoind().await_transaction(txid).await, parent);
	ctx.generate_blocks(6).await;
	let rpc = ctx.bitcoind().sync_client();
	let original = rpc.custom_get_raw_transaction_info(txid, None).unwrap().unwrap();
	let old_hash = original.blockhash.expect("payout must actually confirm before its block is invalidated");
	assert!(original.confirmations.unwrap() >= 6);
	let old_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	wait_for_confirmation(&db, allocation, old_tip, old_hash).await;

	let payout_index = parent.output.iter().position(|out| out.script_pubkey == destination).unwrap() as u32;
	assert_eq!(parent.output[payout_index as usize].value, expected);
	let permanent = db.read(async |t| Ok(t.query_one(
		"SELECT vtxo_id AS id,replannable::TEXT AS facts FROM expiry_allocation_coin WHERE vtxo_id=$1 ORDER BY allocation_id LIMIT 1", &[&ids[0].to_string()],
	).await?)).await.unwrap();
	let entitlement: String = permanent.get("id");
	let irreversible: String = permanent.get("facts");

	// Upstream sync rejects a lower tip. Keep both daemons offline until
	// the real replacement branch is higher than their retained old tip.
	srv.watchmand().stop().await.unwrap();
	srv.stop().await.unwrap();
	rpc.invalidate_block(&old_hash).unwrap();
	let fork = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	assert!(fork.height < old_tip.height);
	assert_eq!(db.read(async |t| Ok(t.get_user_vtxo_by_id(ids[0]).await?.spend_state)).await.unwrap(),
		server::database::SpendState::Spent, "a reorg must not reopen the committed source");

	// Use the same seven replacement blocks and a fresh coinbase destination.
	let mining_address = ctx.bitcoind().get_new_address();
	rpc.generate_to_address(1, &mining_address).unwrap();
	rpc.generate_to_address(6, &mining_address).unwrap();
	let recovery_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	assert!(recovery_tip.height > old_tip.height);
	srv.start().await.unwrap();
	srv.watchmand().start().await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let valid = db.read(async |t| {
				let Some(view) = t.expiry_view().await? else { return Ok(false); };
				Ok(view == recovery_tip
					&& t.get_highest_block(BlockTable::Captaind).await? == Some(recovery_tip)
					&& (t.expiry_view().await? == Some(view)))
			}).await.unwrap();
			// A retained package may already be mined on the replacement
			// branch; otherwise the watchman must recover it in the mempool.
			let published = rpc.get_mempool_entry(&txid).is_ok()
				|| rpc.custom_get_raw_transaction_info(txid, None).unwrap()
					.is_some_and(|info| info.confirmations.unwrap_or(0) > 0);
			if valid && published { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("watchman restart did not rebuild and recover the immutable payout package");

	let new_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	let current = rpc.custom_get_raw_transaction_info(txid, None).unwrap().unwrap();
	let new_hash = current.blockhash.expect("the original payout must re-confirm on the replacement chain");
	assert_ne!(new_hash, old_hash);
	assert!(current.confirmations.unwrap() >= 6);
	wait_for_confirmation(&db, allocation, new_tip, new_hash).await;
	assert_eq!(ctx.bitcoind().await_transaction(txid).await, parent);
	assert_eq!(rpc.get_tx_out(&txid, payout_index, Some(true)).unwrap().unwrap().value, expected);
	let row = db.read(async |t| Ok(t.query_one(
		"SELECT (SELECT replannable::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.vtxo_id=coin.vtxo_id ORDER BY allocation_id LIMIT 1) AS facts, \
			(SELECT COUNT(DISTINCT vtxo_id) FROM expiry_allocation_coin d WHERE d.vtxo_id=coin.vtxo_id) AS fences, \
			(SELECT COUNT(*) FROM expiry_allocation_coin c WHERE c.vtxo_id=coin.vtxo_id AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')) AS active, \
			(SELECT COUNT(DISTINCT a.sweep_txid) FROM expiry_allocation_coin c \
			 JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE c.vtxo_id=coin.vtxo_id AND a.status='confirmed') AS payments, \
			(SELECT sweep_tx FROM expiry_allocation_state WHERE id=$2) AS raw \
		FROM (SELECT $1::TEXT AS vtxo_id) coin", &[&entitlement,&allocation],
	).await?)).await.unwrap();
	assert_eq!(row.get::<_, String>("facts"), irreversible);
	assert_eq!(row.get::<_, i64>("fences"), 1);
	assert_eq!(row.get::<_, i64>("active"), 1);
	assert_eq!(row.get::<_, i64>("payments"), 1);
	assert_eq!(deserialize::<Transaction>(row.get::<_, &[u8]>("raw")).unwrap(), parent,
		"reorg recovery cannot rewrite the immutable payout parent");
}

/// A configured regtest fee rise replaces the direct payout while keeping
/// every owner output fixed and recording both signed attempts.
#[tokio::test]
async fn expiry_payout_rbf_keeps_owner_output_and_tracks_confirmed_fee() {
	let ctx = TestContext::new("server/expiry_payout_fee_replacement").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); }).watchmand().create().await;
	let wallet = ctx.bark("abandoned", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = wallet.client().await;
	let coin = client.get_full_vtxo(ids[0]).await.unwrap();
	let destination = client.fallback_script();
	drop(client);
	// Confirmed watchman funds can sponsor the higher fee in the replacement.
	let fee_address = srv.watchmand().wait_wallet_address().await;
	for _ in 0..2 { ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await; }
	ctx.generate_blocks(2).await;
	let before = ctx.bitcoind().get_block_count().await as u32;
	ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - before).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (allocation, parent) = wait_for_payout(&db, ids[0]).await;
	assert_eq!(parent.version, bitcoin::transaction::Version::TWO);
	let first_fee = actual_fee(&ctx, &parent).await;
	assert!(first_fee > bitcoin::Amount::ZERO);
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	let target = bitcoin::FeeRate::from_sat_per_vb(100).unwrap();
	assert!(first_fee < target * parent.weight());
	let payouts = parent.output.iter().filter(|out| out.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(payouts.len(), 1);
	assert_eq!(payouts[0].value, coin.amount() - sat(2_000));
	srv.watchmand().stop().await.unwrap();
	{
		let mut config = srv.watchmand().config_mut();
		config.fee_estimator.fallback_fee_rate_fast = target;
		config.fee_estimator.fallback_fee_rate_regular = target;
		config.fee_estimator.fallback_fee_rate_slow = target;
	}
	srv.watchmand().start().await.unwrap();
	let rpc = ctx.bitcoind().sync_client();
	let (replacement, recorded_fee) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_one(
				"SELECT a.sweep_tx,p.fee_sat FROM expiry_allocation_state a \
				 JOIN expiry_payout_attempt p ON p.allocation_id=a.id AND p.txid=a.sweep_txid \
				 WHERE a.id=$1", &[&allocation]).await?)).await.unwrap();
			let candidate: Transaction = deserialize(row.get("sweep_tx")).unwrap();
			if candidate != parent && rpc.get_raw_mempool().unwrap().contains(&candidate.compute_txid()) {
				break (candidate, u64::try_from(row.get::<_, i64>("fee_sat")).unwrap());
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("configured fee rise did not replace the real direct payout");
	assert!(!rpc.get_raw_mempool().unwrap().contains(&parent.compute_txid()));
	assert_eq!(replacement.version, bitcoin::transaction::Version::TWO);
	let replacement_payouts = replacement.output.iter().filter(|out| out.script_pubkey == destination)
		.collect::<Vec<_>>();
	assert_eq!(replacement_payouts, payouts, "RBF must preserve the owner's exact output");
	assert!(sat(recorded_fee) >= target * replacement.weight());
	assert!(sat(recorded_fee) > first_fee);
	assert_eq!(recorded_fee, actual_fee(&ctx, &replacement).await.to_sat());
	let attempts = db.read(async |t| Ok(t.query_one(
		"SELECT COUNT(*) FROM expiry_payout_attempt WHERE allocation_id=$1", &[&allocation]
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(attempts, 2, "both signed attempts must survive replacement");
	ctx.generate_blocks(6).await;
	let tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	let info = rpc.custom_get_raw_transaction_info(replacement.compute_txid(), None).unwrap().unwrap();
	assert!(info.confirmations.unwrap_or(0) >= 6);
	wait_for_confirmation(&db, allocation, tip, info.blockhash.unwrap()).await;
	let confirmed = db.read(async |t| Ok(t.query_one(
		"SELECT sweep_txid FROM expiry_allocation_state WHERE id=$1", &[&allocation]
	).await?.get::<_, String>(0))).await.unwrap();
	assert_eq!(confirmed, replacement.compute_txid().to_string());
}

/// A fee estimate above the configured ceiling cannot publish a payout.
/// Raising the ceiling lets the same owner decision pay.
#[tokio::test]
async fn expiry_payout_above_fee_rate_ceiling_is_refused() {
	let ctx = TestContext::new("server/expiry_payout_fee_rate_ceiling").await;
	let ceiling = bitcoin::FeeRate::from_sat_per_vb(5).unwrap();
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(move |cfg| {
			// The regular fallback estimate is 7 sat/vB.
			cfg.watchman.expiry_payout_max_fee_rate = ceiling;
		}).create().await;
	let wallet = ctx.bark("abandoned", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let coin = wallet.client().await.get_full_vtxo(ids[0]).await.unwrap();
	let before = ctx.bitcoind().get_block_count().await as u32;
	ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - before).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let allocation = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_opt(
				"SELECT allocation_id FROM expiry_allocation_coin WHERE vtxo_id=$1",
				&[&coin.id().to_string()]).await?)).await.unwrap();
			if let Some(row) = row { break row.get::<_, i64>(0); }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the owner decision was not stored");
	// Several sweep passes run, and each refuses the attempt.
	tokio::time::sleep(Duration::from_secs(5)).await;
	let attempts = db.read(async |t| Ok(t.query_one(
		"SELECT COUNT(*) FROM expiry_payout_attempt WHERE allocation_id=$1", &[&allocation]
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(attempts, 0, "a payout above the fee-rate ceiling was stored");

	srv.watchmand().stop().await.unwrap();
	srv.watchmand().config_mut().watchman.expiry_payout_max_fee_rate = bitcoin::FeeRate::from_sat_per_vb(1000).unwrap();
	srv.watchmand().start().await.unwrap();
	let (paid, payout) = wait_for_payout(&db, coin.id()).await;
	assert_eq!(paid, allocation);
	assert!(actual_fee(&ctx, &payout).await > ceiling * payout.weight());
}

/// A crash cannot erase a held owner decision. A high fee target needs a
/// confirmed watchman input in the same direct payout transaction.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn restart_unfunded_allocation_waits_for_trusted_sponsor_then_pays_owner() {
	let ctx = TestContext::new("scuttle/direct_payout_trusted_sponsor_recovery").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(|cfg| {
			cfg.min_trusted_confs = 2;
			cfg.watchman.sweep_interval = Duration::from_secs(1);
			cfg.fee_estimator.fallback_fee_rate_regular = bitcoin::FeeRate::from_sat_per_vb(30).unwrap();
		}).create().await;
	let wallet = ctx.bark("absent_owner", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = wallet.client().await;
	let coin = client.get_full_vtxo(ids[0]).await.unwrap();
	let destination = client.fallback_script();
	drop(client);
	drop(wallet);
	let height = ctx.bitcoind().get_block_count().await as u32;
	ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - height).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (allocation, entitlement, facts) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_opt(
				"SELECT a.id,c.vtxo_id AS entitlement,c.replannable::TEXT AS facts,a.status,a.sweep_tx,a.broadcast_at \
				 FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				 WHERE c.vtxo_id=$1 AND a.status<>'conflicted'",
				 &[&ids[0].to_string()]).await?)).await.unwrap();
			if let Some(row) = row {
				assert_eq!(row.get::<_, &str>("status"), "pending");
				assert!(row.get::<_, Option<&[u8]>>("sweep_tx").is_none());
				assert!(row.get::<_, Option<chrono::DateTime<chrono::Utc>>>("broadcast_at").is_none());
				break (row.get::<_, i64>("id"), row.get::<_, String>("entitlement"),
					row.get::<_, String>("facts"));
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("unfunded owner decision was not retained");
	srv.watchmand().crash_stop().await.unwrap();
	srv.watchmand().start().await.unwrap();
	let fee_address = srv.watchmand().wait_wallet_address().await;
	let deposit = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
	let deposit_tx = ctx.bitcoind().await_transaction(deposit).await;
	let fee_vout = deposit_tx.output.iter().position(|out| out.script_pubkey == fee_address.script_pubkey()).unwrap() as u32;
	let fee_point = bitcoin::OutPoint::new(deposit, fee_vout);
	ctx.generate_blocks(1).await;
	let one_conf = ctx.bitcoind().sync_client().get_tx_out(&deposit, fee_vout, Some(true)).unwrap().unwrap();
	assert_eq!(one_conf.confirmations, 1);
	srv.watchmand().wait_for_sync_height(bitcoin_ext::BlockHeight::new(
		ctx.bitcoind().get_block_count().await as u32)).await;
	srv.watchmand().trigger_sweep().await;
	tokio::time::sleep(Duration::from_secs(2)).await;
	let held = db.read(async |t| Ok(t.query_one(
		"SELECT a.sweep_tx,a.status,c.replannable::TEXT AS facts FROM expiry_allocation_state a \
		 JOIN expiry_allocation_coin c ON c.allocation_id=a.id WHERE a.id=$1 AND c.vtxo_id=$2",
		 &[&allocation,&entitlement]).await?)).await.unwrap();
	assert_eq!(held.get::<_, &str>("status"), "pending");
	assert!(held.get::<_, Option<&[u8]>>("sweep_tx").is_none(), "one confirmation is not trusted");
	assert_eq!(held.get::<_, String>("facts"), facts);
	ctx.generate_blocks(1).await;
	let (published, payout) = wait_for_payout(&db, ids[0]).await;
	assert_eq!(published, allocation);
	assert_eq!(payout.version, bitcoin::transaction::Version::TWO);
	assert!(payout.input.iter().any(|input| input.previous_output == coin.chain_anchor()));
	assert!(payout.input.iter().any(|input| input.previous_output == fee_point));
	assert_eq!(payout.output.iter().filter(|out| out.script_pubkey == destination)
		.map(|out| out.value).collect::<Vec<_>>(), vec![sat(98_000)]);
	assert!(payout.output.iter().all(|out| !out.is_p2a_fee_anchor()));
	let miner_fee = actual_fee(&ctx, &payout).await;
	assert!(miner_fee > sat(2_000));
	assert_eq!(payout.output.iter().map(|out| out.value).sum::<bitcoin::Amount>() + miner_fee,
		sat(200_000));
	let recorded = db.read(async |t| Ok(t.query_one(
		"SELECT COUNT(*) AS attempts,MAX(fee_sat) AS fee_sat FROM expiry_payout_attempt WHERE allocation_id=$1",
		 &[&allocation]).await?)).await.unwrap();
	assert_eq!(recorded.get::<_, i64>("attempts"), 1);
	assert_eq!(recorded.get::<_, i64>("fee_sat"), i64::try_from(miner_fee.to_sat()).unwrap());
	ctx.generate_blocks(6).await;
	let tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	let info = ctx.bitcoind().sync_client().custom_get_raw_transaction_info(payout.compute_txid(), None).unwrap().unwrap();
	assert!(info.confirmations.unwrap_or(0) >= 6);
	wait_for_confirmation(&db, allocation, tip, info.blockhash.unwrap()).await;
	let final_row = db.read(async |t| Ok(t.query_one(
		"SELECT a.sweep_txid,c.fee_sat,c.payout_sat,c.forfeited_sat,c.replannable::TEXT AS facts \
		 FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
		 WHERE a.id=$1 AND c.vtxo_id=$2",
		 &[&allocation,&entitlement]).await?)).await.unwrap();
	assert_eq!(final_row.get::<_, String>("sweep_txid"), payout.compute_txid().to_string());
	assert_eq!(final_row.get::<_, i64>("fee_sat"), 2_000);
	assert_eq!(final_row.get::<_, i64>("payout_sat"), 98_000);
	assert_eq!(final_row.get::<_, i64>("forfeited_sat"), 0);
	assert_eq!(final_row.get::<_, String>("facts"), facts);
}
