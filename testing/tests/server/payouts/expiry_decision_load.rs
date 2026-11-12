//! Regression for the 2026-11-07 stall: one large same-expiry group.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ark_testing::{sat, TestContext};
use ark_testing::constants::BOARD_CONFIRMATIONS;
use bitcoin_ext::BlockDelta;
use server::database::Db;

/// The decision analyses a large group without locks, then holds the group's
/// branch rows only for its short checked write. Lightning admissions, which
/// take the exclusive settlement lock, keep being served, and no backend
/// waits on a lock for long. Every coin gets one entitlement and pays its owner.
#[tokio::test]
async fn large_expiry_group_leaves_captaind_serving() {
	const WALLETS: usize = 3;
	const BOARDS: usize = 20;
	let ctx = TestContext::new("server/large_expiry_group_leaves_captaind_serving").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(30); }).watchmand().create().await;
	let mut wallets = Vec::new();
	for index in 0..WALLETS {
		wallets.push(ctx.bark(format!("owner{index}"), &srv).funded(sat(BOARDS as u64 * 25_000))
			.cfg(|cfg| cfg.daemon_manual_sync = true).create().await);
	}
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(1_000_000)).await;
	// Every board confirms in the same block, so all roots share one expiry.
	let mut boards = Vec::new();
	for wallet in &wallets {
		for _ in 0..BOARDS { boards.push(wallet.board(sat(20_000)).await); }
	}
	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	let mut coins = Vec::new();
	for (index, wallet) in wallets.iter().enumerate() {
		wallet.sync().await;
		let client = wallet.client().await;
		for board in &boards[index * BOARDS..(index + 1) * BOARDS] {
			let coin = client.get_full_vtxo(board.vtxos[0]).await.unwrap();
			coins.push((coin.id(), coin.expiry_height(), client.fallback_script()));
		}
	}
	let expiry = coins[0].1;
	assert!(coins.iter().all(|coin| coin.1 == expiry));

	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let done = Arc::new(AtomicBool::new(false));
	// Sampled every 5 ms: the longest-running decision write (the only writer
	// here that holds vtxo row locks), whether it held the settlement lock,
	// and the longest lock wait of any backend.
	let sampler = tokio::spawn({
		let (db, done) = (db.clone(), done.clone());
		async move {
			let (mut hold, mut settlement, mut wait) = (0_f64, false, 0_f64);
			while !done.load(Ordering::Relaxed) {
				let row = db.read(async |t| Ok(t.query_one("WITH decision AS (SELECT a.pid,a.xact_start FROM pg_locks l \
						JOIN pg_stat_activity a ON a.pid=l.pid WHERE l.locktype='relation' AND l.relation='vtxo'::regclass \
						AND l.mode='RowShareLock' AND l.granted AND a.pid<>pg_backend_pid()) SELECT \
					COALESCE((SELECT MAX(EXTRACT(EPOCH FROM clock_timestamp()-xact_start)) FROM decision),0)::FLOAT8, \
					EXISTS(SELECT 1 FROM decision d JOIN pg_locks l ON l.pid=d.pid WHERE l.locktype='advisory' \
						AND l.classid=0 AND l.objid=2 AND l.objsubid=1 AND l.granted), \
					COALESCE((SELECT MAX(EXTRACT(EPOCH FROM clock_timestamp()-query_start)) FROM pg_stat_activity \
						WHERE wait_event_type='Lock' AND datname=current_database()),0)::FLOAT8",
					&[]).await?)).await.unwrap();
				hold = hold.max(row.get(0));
				settlement |= row.get::<_, bool>(1);
				wait = wait.max(row.get(2));
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
			(hold, settlement, wait)
		}
	});
	// The exclusive settlement lock is what every Lightning send admission,
	// receive grant and hold-invoice settlement takes.
	let admission = tokio::spawn({
		let (db, done) = (db.clone(), done.clone());
		async move {
			let mut slowest = Duration::ZERO;
			while !done.load(Ordering::Relaxed) {
				let start = Instant::now();
				db.write(async |t| Ok(t.execute("SELECT pg_advisory_xact_lock(2)", &[]).await?)).await.unwrap();
				slowest = slowest.max(start.elapsed());
				tokio::time::sleep(Duration::from_millis(10)).await;
			}
			slowest
		}
	});

	let height = ctx.bitcoind().get_block_count().await as u32;
	ctx.generate_blocks(expiry.to_u32() + 1 - height).await;
	let ids = coins.iter().map(|coin| coin.0.to_string()).collect::<Vec<_>>();
	let started = Instant::now();
	tokio::time::timeout(Duration::from_secs(120), async {
		for round in 0.. {
			let decided = db.read(async |t| Ok(t.query_one("SELECT COUNT(DISTINCT vtxo_id) FROM expiry_allocation_coin WHERE vtxo_id=ANY($1)",
				&[&ids]).await?.get::<_, i64>(0))).await.unwrap();
			if decided == ids.len() as i64 { break; }
			tokio::time::sleep(Duration::from_millis(500)).await;
			if round % 4 == 3 { ctx.generate_blocks(1).await; }
		}
	}).await.expect("every coin of the large group must be decided");
	let decided_after = started.elapsed();
	done.store(true, Ordering::Relaxed);
	let (hold, settlement, wait) = sampler.await.unwrap();
	let slowest_admission = admission.await.unwrap();
	println!("N2 roots={} decided_after={decided_after:?} decision_write_max={hold:.3}s decision_held_settlement={settlement} \
		lock_wait_max={wait:.3}s settlement_lock_max={slowest_admission:?}", coins.len());

	for (id, _, script) in &coins {
		let rows = db.read(async |t| Ok(t.query("SELECT c.disposition,c.owner_mailbox FROM expiry_allocation_coin c \
			WHERE c.vtxo_id=$1", &[&id.to_string()]).await?))
			.await.unwrap();
		assert_eq!(rows.len(), 1, "one entitlement and one allocation for {id}");
		assert_eq!(rows[0].get::<_, &str>("disposition"), "customer");
		let mailbox = rows[0].get::<_, &str>("owner_mailbox").parse().unwrap();
		assert_eq!(&ark::fallback::payout_script(mailbox), script);
	}
	assert!(hold > 0.0, "the sampler must observe the decision write");
	assert!(!settlement, "a group without Lightning evidence must not block Lightning admissions");
	assert!(hold < 1.0, "decision write held the group's rows for {hold:.3}s");
	assert!(wait < 1.0, "a backend waited {wait:.3}s on a lock");
	assert!(slowest_admission < Duration::from_secs(1), "settlement lock waited {slowest_admission:?}");
}
