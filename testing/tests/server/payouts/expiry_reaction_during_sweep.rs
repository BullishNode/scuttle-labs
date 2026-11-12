use std::time::Duration;

use ark_testing::constants::ROUND_CONFIRMATIONS;
use ark_testing::exit::progress_exit_until_awaiting_delta;
use ark_testing::{TestContext, btc, require_bark_version, sat};
use bitcoin_ext::{BlockDelta, rpc::RpcApi};
use server::database::Db;
use server_log::ProgressBroadcast;

/// A stalled expiry decision must not stop a signed arkoor receiver step from
/// reaching Core. The delay is a PostgreSQL trigger in this test database;
/// the watchman runs its ordinary event loop and wallet publication path.
#[tokio::test]
async fn exposed_progress_broadcasts_while_expiry_sweep_is_stalled() {
	require_bark_version!(> "0.5.0");

	let ctx = TestContext::new("server/exposed_progress_during_expiry_sweep").await;
	let srv = ctx
		.captaind("server")
		.funded(btc(10))
		.bitcoind(ctx.bitcoind_arc())
		.no_vtxo_pool()
		.cfg(|cfg| {
			cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(40);
			cfg.vtxo_exit_delta = BlockDelta::new(6);
		})
		.watchmand_cfg(|cfg| {
			cfg.watchman.reaction_interval = Duration::from_millis(250);
			cfg.watchman.sweep_interval = Duration::from_secs(600);
		})
		.create()
		.await;
	let wm = srv.watchmand();

	// The first board expires well before the arkoor sender's later board.
	let abandoned = ctx
		.bark("abandoned", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true)
		.funded(sat(200_000))
		.create()
		.await;
	let old_ids = abandoned
		.board_and_confirm_and_register(&ctx, sat(100_000))
		.await;
	assert_eq!(old_ids.len(), 1);
	let old_client = abandoned.client().await;
	let old_coin = old_client.get_full_vtxo(old_ids[0]).await.unwrap();
	drop(old_client);
	ctx.generate_blocks(18).await;

	let sender = ctx.bark("sender", &srv).funded(sat(500_000)).create().await;
	let receiver = ctx
		.bark("receiver", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true)
		.create()
		.await;
	sender
		.board_and_confirm_and_register(&ctx, sat(300_000))
		.await;
	ctx.refresh_all(&srv, &[&sender]).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	sender.sync().await;
	let sender_coin = sender.vtxo_ids().await[0];
	let stale_sender = sender.full_clone("stale_sender").await;
	sender.send_oor(receiver.address().await, sat(50_000)).await;
	receiver.sync().await;
	let receiver_client = receiver.client().await;
	let received = receiver_client.vtxos().await.unwrap();
	assert_eq!(received.len(), 1);
	let received = receiver_client
		.get_full_vtxo(received[0].id())
		.await
		.unwrap();
	assert_eq!(received.amount(), sat(50_000));
	assert!(received.expiry_height() > old_coin.expiry_height());
	drop(receiver_client);

	let sponsor = wm.wait_wallet_address().await;
	ctx.bitcoind().fund_addr(sponsor, sat(100_000)).await;
	let funded_tip = ctx.generate_blocks(2).await;
	wm.wait_for_sync_height(funded_tip).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let old_expiry = old_coin.expiry_height().to_u32();
	let current = ctx.bitcoind().get_block_count().await as u32;
	assert!(
		current < old_expiry,
		"the later arkoor setup must precede the old board expiry"
	);
	db.write(async |tx| {
		let function = format!(
			r#"CREATE FUNCTION test_stall_old_expiry() RETURNS trigger
			LANGUAGE plpgsql AS $$ BEGIN
			IF NEW.expiry_height = {old_expiry} THEN PERFORM pg_sleep(90); END IF;
			RETURN NEW;
			END $$"#
		);
		tx.execute(&function, &[]).await?;
		tx.execute(
			"CREATE TRIGGER test_stall_old_expiry BEFORE INSERT ON expiry_allocation \
			FOR EACH ROW EXECUTE FUNCTION test_stall_old_expiry()",
			&[],
		)
		.await?;
		Ok(())
	})
	.await
	.unwrap();

	let expired = ctx.generate_blocks(old_expiry + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	wm.wait_for_sync_height(expired).await;
	let sleeping_pid = tokio::time::timeout(Duration::from_secs(20), async {
		loop {
			let pid = db
				.read(async |tx| {
					Ok(tx
						.query_opt(
							"SELECT pid FROM pg_stat_activity \
				WHERE datname=current_database() AND wait_event='PgSleep' LIMIT 1",
							&[],
						)
						.await?
						.map(|row| row.get::<_, i32>(0)))
				})
				.await
				.unwrap();
			if let Some(pid) = pid {
				break pid;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	})
	.await
	.expect("the real expiry sweep never reached the deliberate database stall");

	// The stale clone exits the round coin it held before the OOR transfer.
	// Only watchman's reactive progression can publish the signed successor.
	let mut progress_logs = wm.subscribe_log::<ProgressBroadcast>();
	stale_sender.start_exit_all().await;
	let core = ctx.bitcoind().sync_client();
	let observe = async {
		loop {
			if let Ok(progress) = progress_logs.try_recv() {
				assert_eq!(progress.vtxo_id, sender_coin);
				assert!(core.get_raw_transaction(&progress.txid, None).is_ok());
				let still_stalled = db
					.read(async |tx| {
						Ok(tx
							.query_one(
								"SELECT EXISTS( \
					SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event='PgSleep')",
								&[&sleeping_pid],
							)
							.await?
							.get::<_, bool>(0))
					})
					.await
					.unwrap();
				assert!(
					still_stalled,
					"receiver progression waited for the expiry sweep to finish"
				);
				break;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	};
	let exit_and_reach_progress_grace = async {
		progress_exit_until_awaiting_delta(&ctx, &stale_sender).await;
		let near_deadline = ctx
			.generate_blocks(wm.config().watchman.progress_grace_period.to_u32())
			.await;
		wm.wait_for_sync_height(near_deadline).await;
	};
	tokio::time::timeout(Duration::from_secs(65), async {
		tokio::join!(exit_and_reach_progress_grace, observe);
	})
	.await
	.expect("signed receiver progression did not reach Core during the stalled sweep");

	// End only the test's synthetic delay; the expiry transaction rolls back
	// and the isolated test database is discarded at teardown.
	let cancelled = db
		.write(async |tx| {
			Ok(tx
				.query_one("SELECT pg_cancel_backend($1)", &[&sleeping_pid])
				.await?
				.get::<_, bool>(0))
		})
		.await
		.unwrap();
	assert!(cancelled);
}
