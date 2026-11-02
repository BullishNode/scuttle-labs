use super::expiry_test::{actual_fee, canonical, wait_payout as wait_for_payout};

use std::time::Duration;

use ark_testing::{sat, TestContext};
use bitcoin::{BlockHash, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin_ext::{BlockDelta, BlockRef, TxOutExt};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::{BlockTable, Db};
use server_rpc::admin::WalletAdminServiceClient;
use server_rpc::protos;

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
	let expected = coin.amount() - sat(1_000);
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
		"SELECT e.id,(SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts FROM expiry_entitlement e WHERE e.vtxo_id=$1", &[&ids[0].to_string()],
	).await?)).await.unwrap();
	let entitlement: i64 = permanent.get("id");
	let irreversible: String = permanent.get("facts");

	// The server observes the shorter fork while the watchman is offline.
	// Its old completed view must not authorize work for the new generation.
	srv.watchmand().stop().await.unwrap();
	rpc.invalidate_block(&old_hash).unwrap();
	let fork = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	assert!(fork.height < old_tip.height);
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let ready = db.read(async |t| t.get_highest_block(BlockTable::Captaind).await).await.unwrap() == Some(fork);
			if ready { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual shorter fork was not indexed before watchman restart");
	assert_eq!(db.read(async |t| Ok(t.get_user_vtxo_by_id(ids[0]).await?.spend_state)).await.unwrap(),
		server::database::SpendState::Spent, "a reorg must not reopen the committed source");

	srv.watchmand().start().await.unwrap();
	// Upstream's wallet processes a reorg on the next block. Mine one of
	// the replacement blocks before asking it to rebuild the fee child.
	// A fresh coinbase destination prevents reproducing the invalid block.
	let mining_address = ctx.bitcoind().get_new_address();
	rpc.generate_to_address(1, &mining_address).unwrap();
	let recovery_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let valid = db.read(async |t| {
				let Some(view) = t.expiry_view().await? else { return Ok(false); };
				Ok(view == recovery_tip
					&& (t.expiry_view().await? == Some(view)))
			}).await.unwrap();
			// A retained package may already be mined in the first replacement
			// block; otherwise the watchman must recover it in the mempool.
			let published = rpc.get_mempool_entry(&txid).is_ok()
				|| rpc.custom_get_raw_transaction_info(txid, None).unwrap()
					.is_some_and(|info| info.confirmations.unwrap_or(0) > 0);
			if valid && published { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("watchman restart did not rebuild and recover the immutable payout package");

	rpc.generate_to_address(6, &mining_address).unwrap();
	let new_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	let current = rpc.custom_get_raw_transaction_info(txid, None).unwrap().unwrap();
	let new_hash = current.blockhash.expect("the original payout must re-confirm on the replacement chain");
	assert_ne!(new_hash, old_hash);
	assert!(current.confirmations.unwrap() >= 6);
	wait_for_confirmation(&db, allocation, new_tip, new_hash).await;
	assert_eq!(ctx.bitcoind().await_transaction(txid).await, parent);
	assert_eq!(rpc.get_tx_out(&txid, payout_index, Some(true)).unwrap().unwrap().value, expected);
	let row = db.read(async |t| Ok(t.query_one(
		"SELECT (SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts, \
			(SELECT COUNT(*) FROM expiry_entitlement d WHERE d.id=e.id) AS fences, \
			(SELECT COUNT(*) FROM expiry_allocation_coin c WHERE c.entitlement_id=e.id AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')) AS active, \
			(SELECT COUNT(DISTINCT a.sweep_txid) FROM expiry_allocation_coin c \
			 JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE c.entitlement_id=e.id AND a.status='confirmed') AS payments, \
			(SELECT sweep_tx FROM expiry_allocation_state WHERE id=$2) AS raw \
		FROM expiry_entitlement e WHERE e.id=$1", &[&entitlement,&allocation],
	).await?)).await.unwrap();
	assert_eq!(row.get::<_, String>("facts"), irreversible);
	assert_eq!(row.get::<_, i64>("fences"), 1);
	assert_eq!(row.get::<_, i64>("active"), 1);
	assert_eq!(row.get::<_, i64>("payments"), 1);
	assert_eq!(deserialize::<Transaction>(row.get::<_, &[u8]>("raw")).unwrap(), parent,
		"reorg recovery cannot rewrite the immutable payout parent");
}

/// A configured regtest fee rise drives the real watchman replacement path.
/// This is a fee-policy/restart test, not a congested public-network claim.
#[tokio::test]
async fn expiry_payout_fee_replacement_keeps_parent_and_counts_only_confirmed_child() {
	let ctx = TestContext::new("server/expiry_payout_fee_replacement").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); }).watchmand().create().await;
	let wallet = ctx.bark("abandoned", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = wallet.client().await;
	let coin = client.get_full_vtxo(ids[0]).await.unwrap();
	let destination = client.fallback_script();
	drop(client);
	// Two confirmed funding outputs allow a replacement to select
	// another usable sponsor while the first child remains in the mempool.
	let fee_address = srv.watchmand().wait_wallet_address().await;
	for _ in 0..2 { ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await; }
	ctx.generate_blocks(2).await;
	let before = ctx.bitcoind().get_block_count().await as u32;
	ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - before).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (allocation, parent) = wait_for_payout(&db, ids[0]).await;
	let (first_child, first_fee) = ark_testing::onchain::observed_payout_fee_child(&ctx, &parent).unwrap();
	let first_fee = u64::try_from(first_fee).unwrap();
	assert_eq!(ctx.bitcoind().await_transaction(first_child.compute_txid()).await, first_child);
	let target = bitcoin::FeeRate::from_sat_per_vb(100).unwrap();
	assert!(sat(first_fee) < target * (parent.weight() + first_child.weight()));
	let payouts = parent.output.iter().filter(|out| out.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(payouts.len(), 1);
	assert_eq!(payouts[0].value, coin.amount() - sat(1_000));
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
			let saved: Vec<u8> = db.read(async |t| Ok(t.query_one("SELECT sweep_tx FROM expiry_allocation_state WHERE id=$1",
				&[&allocation]).await?.get(0))).await.unwrap();
			assert_eq!(deserialize::<Transaction>(&saved).unwrap(), parent, "fee replacement must never change the payout parent");
			let Some((child, measured_fee)) = ark_testing::onchain::observed_payout_fee_child(&ctx, &parent) else {
				tokio::time::sleep(Duration::from_millis(50)).await; continue;
			};
			if child != first_child && rpc.get_mempool_entry(&child.compute_txid()).is_ok() {
				break (child, u64::try_from(measured_fee).unwrap());
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("configured fee rise did not replace the real mempool fee child");
	assert!(rpc.get_mempool_entry(&first_child.compute_txid()).is_err());
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	assert!(sat(recorded_fee) >= target * (parent.weight() + replacement.weight()));
	assert!(recorded_fee > first_fee && recorded_fee > 1_000);
	assert_eq!(recorded_fee, actual_fee(&ctx, &replacement).await.to_sat());
	ctx.generate_blocks(6).await;
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if let Some((child, fee)) = ark_testing::onchain::observed_payout_fee_child(&ctx, &parent) {
				let info = rpc.custom_get_raw_transaction_info(child.compute_txid(), None).unwrap().unwrap();
				if info.confirmations.unwrap_or(0) >= 6 {
					assert_eq!(child, replacement);
					assert_eq!(u64::try_from(fee).unwrap(), recorded_fee);
					break;
				}
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual canonical fee child confirmation was not accounted");
}

/// A positive external deposit is visible after one real confirmation, but
/// remains untrusted under the explicitly configured two-confirmation policy.
/// The next block makes that SAME deposit usable; only the configured sweeper
/// may publish the already committed immutable payout. This is not a test of
/// incoming mempool ingestion or of the default one-confirmation policy.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn crash_after_commit_before_confirmation_retries_same_parent() {
	Box::pin(positive_untrusted_parent_recovery()).await;
}

#[cfg(target_os = "linux")]
async fn positive_untrusted_parent_recovery() {
	let ctx = TestContext::new("scuttle/expiry_parent_commit_crash_recovery").await;
	let srv = Box::pin(ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(|cfg| {
			cfg.min_trusted_confs = 2;
			cfg.watchman.sweep_interval = Duration::from_secs(1);
		}).create()).await;
	assert_eq!(srv.watchmand().config().min_trusted_confs, 2);
	assert_eq!(srv.watchmand().config().watchman.sweep_interval, Duration::from_secs(1));
	let wallet = Box::pin(ctx.bark("absent_owner", &srv).funded(sat(200_000)).create()).await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	assert_eq!(ids.len(), 1);
	let client = wallet.client().await;
	let coin = client.get_full_vtxo(ids[0]).await.unwrap();
	let destination = client.fallback_script();
	assert_eq!(coin.amount(), sat(100_000));
	drop(client);
	drop(wallet);

	let fee_address = srv.watchmand().wait_wallet_address().await;
	let admin_address = srv.watchmand().config().admin_address.unwrap();
	let mut admin = WalletAdminServiceClient::connect(format!("http://{admin_address}")).await.unwrap();
	let initial = admin.wallet_status(protos::Empty {}).await.unwrap().into_inner().watchman.unwrap();
	assert_eq!(initial.total_balance, 0);
	assert_eq!(initial.trusted_balance, 0);
	let height = ctx.bitcoind().get_block_count().await as u32;
	assert!(height < coin.expiry_height().to_u32());
	ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - height).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (allocation, entitlement, facts, parent) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_opt("SELECT a.id,e.id AS entitlement,(SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts,a.sweep_tx, \
				a.status,a.broadcast_at IS NULL AS unpublished FROM expiry_allocation_state a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND a.status<>'conflicted' AND a.sweep_tx IS NOT NULL", &[&ids[0].to_string()]).await?)).await.unwrap();
			if let Some(row) = row {
				assert_eq!(row.get::<_, String>("status"), "pending");
				assert!(row.get::<_, bool>("unpublished"));
				break (row.get::<_, i64>("id"), row.get::<_, i64>("entitlement"),
					row.get::<_, String>("facts"), deserialize::<Transaction>(row.get("sweep_tx")).unwrap());
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("unfunded rightful allocation must retain its real signed parent");
	assert_eq!(parent.input.len(), 1);
	let backing_point = parent.input[0].previous_output;
	assert_eq!(backing_point, coin.chain_anchor(), "plain board payout must spend its own signed funding anchor");
	let backing_tx = ctx.bitcoind().await_transaction(backing_point.txid).await;
	assert_eq!(backing_tx.compute_txid(), backing_point.txid);
	coin.validate(&backing_tx).unwrap();
	let backing_output = &backing_tx.output[backing_point.vout as usize];
	assert_eq!(backing_output.value, sat(100_000));
	let actual_backing = ctx.bitcoind().sync_client()
		.get_tx_out(&backing_point.txid, backing_point.vout, Some(true)).unwrap().unwrap();
	assert!(actual_backing.confirmations > 0);
	assert_eq!(actual_backing.value, backing_output.value);
	let payees = parent.output.iter().enumerate().filter(|(_, out)| out.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(payees.len(), 1);
	let payout_index = payees[0].0 as u32;
	assert_eq!(payees[0].1.value, sat(99_000));
	assert_eq!(parent.output.iter().map(|out| out.value).sum::<bitcoin::Amount>(), sat(100_000));
	let anchors = parent.output.iter().enumerate().filter(|(_, out)| out.is_p2a_fee_anchor()).collect::<Vec<_>>();
	assert_eq!(anchors.len(), 1);
	assert_eq!(anchors[0].1.value, sat(0));
	let anchor = bitcoin::OutPoint::new(parent.compute_txid(), anchors[0].0 as u32);
	let operator_spk = {
		let config = srv.watchmand().config();
		config.sweep_address.clone().unwrap().require_network(config.network).unwrap().script_pubkey()
	};
	assert_ne!(destination, operator_spk);
	assert_eq!(parent.output.iter().filter(|out| out.script_pubkey == operator_spk)
		.map(|out| out.value).sum::<bitcoin::Amount>(), sat(1_000));
	let before = srv.watchmand().native_child().await.unwrap();
	srv.watchmand().crash_stop().await.unwrap();
	assert!(ctx.bitcoind().sync_client().get_mempool_entry(&parent.compute_txid()).is_err());
	srv.watchmand().start().await.unwrap();
	assert_ne!(srv.watchmand().native_child().await.unwrap(), before);
	let row = db.read(async |t| Ok(t.query_one("SELECT a.sweep_tx,a.status,a.broadcast_at IS NULL AS unpublished, \
		(SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts FROM expiry_allocation_state a,expiry_entitlement e WHERE a.id=$1 AND e.id=$2",
		&[&allocation, &entitlement]).await?)).await.unwrap();
	assert_eq!(deserialize::<Transaction>(row.get("sweep_tx")).unwrap(), parent);
	assert_eq!(row.get::<_, String>("status"), "pending");
	assert!(row.get::<_, bool>("unpublished"));
	assert_eq!(row.get::<_, String>("facts"), facts);
	let address = srv.watchmand().config().admin_address.unwrap();
	admin = WalletAdminServiceClient::connect(format!("http://{address}")).await.unwrap();


	let deposit = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
	let deposit_tx = ctx.bitcoind().await_transaction(deposit).await;
	let deposit_outputs = deposit_tx.output.iter().enumerate()
		.filter(|(_, out)| out.script_pubkey == fee_address.script_pubkey()).collect::<Vec<_>>();
	assert_eq!(deposit_outputs.len(), 1);
	assert_eq!(deposit_outputs[0].1.value, sat(100_000));
	let fee_point = bitcoin::OutPoint::new(deposit, deposit_outputs[0].0 as u32);
	ctx.generate_blocks(1).await;
	let rpc = ctx.bitcoind().sync_client();
	let unusable = rpc.get_tx_out(&deposit, fee_point.vout, Some(true)).unwrap().unwrap();
	assert_eq!(unusable.confirmations, 1);
	assert_eq!(unusable.value, sat(100_000));
	tokio::time::timeout(Duration::from_secs(15), async {
		loop {
			let status = admin.wallet_status(protos::Empty {}).await.unwrap().into_inner().watchman.unwrap();
			if status.total_balance == 100_000 && status.untrusted_balance == 100_000 {
				assert_eq!(status.trusted_balance, 0);
				assert_eq!(status.confirmed_utxos, vec![fee_point.to_string()]);
				assert!(status.unconfirmed_utxos.is_empty());
				break status;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("real positive deposit must be visible but not trusted after one confirmation");

	// Subscribe only AFTER the positive balance was observed. A startup warning
	// from the earlier empty wallet cannot satisfy this HOLD oracle.
	let positive_at = chrono::Utc::now();
	let (warning_tx, mut warning_rx) = tokio::sync::mpsc::unbounded_channel();
	srv.watchmand().add_stdout_handler(move |line: &str| {
		if let Ok(record) = server_log::parse_record(line) {
			if record.message == "Expiry payouts need confirmed usable watchman funds"
				&& record.level == "WARN"
				&& record.timestamp >= positive_at
				&& record.extra.get("allocation_id").and_then(|value| value.as_i64()) == Some(allocation)
			{
				let _ = warning_tx.send(record.timestamp.to_rfc3339());
				return true;
			}
		}
		false
	});
	tokio::time::timeout(Duration::from_secs(10), warning_rx.recv()).await
		.expect("configured sweeper must warn for this positive-but-untrusted allocation")
		.expect("watchman warning handler closed");
	let held = db.read(async |t| Ok(t.query_one("SELECT a.status,a.sweep_tx,a.broadcast_at IS NULL AS unpublished, \
		(SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts FROM expiry_allocation_state a,expiry_entitlement e \
		WHERE a.id=$1 AND e.id=$2", &[&allocation,&entitlement]).await?)).await.unwrap();
	assert_eq!(held.get::<_, String>("status"), "pending");
	assert!(held.get::<_, bool>("unpublished"));
	assert_eq!(held.get::<_, String>("facts"), facts);
	assert_eq!(deserialize::<Transaction>(held.get("sweep_tx")).unwrap(), parent);
	assert!(rpc.get_mempool_entry(&parent.compute_txid()).is_err());
	assert_eq!(rpc.get_tx_out(&deposit, fee_point.vout, Some(true)).unwrap().unwrap().confirmations, 1);

	// One further real confirmation meets the declared policy. No trigger,
	// restart, client call, or financial SQL may substitute for timer recovery.
	ctx.generate_blocks(1).await;
	let eligible = rpc.get_tx_out(&deposit, fee_point.vout, Some(false)).unwrap().unwrap();
	assert_eq!(eligible.confirmations, 2);
	let (published, actual_parent) = wait_for_payout(&db, ids[0]).await;
	assert_eq!((published, actual_parent), (allocation, parent.clone()));
	let (child, measured_fee) = ark_testing::onchain::observed_payout_fee_child(&ctx, &parent).unwrap();
	assert_eq!(child.input.len(), 2);
	assert_eq!(child.input.iter().map(|input| input.previous_output).collect::<std::collections::BTreeSet<_>>(),
		[anchor, fee_point].into_iter().collect::<std::collections::BTreeSet<_>>());
	assert_eq!(ctx.bitcoind().await_transaction(child.compute_txid()).await, child);
	let fee = sat(100_000) - child.output.iter().map(|out| out.value).sum::<bitcoin::Amount>();
	assert!(fee > sat(0));
	assert_eq!(i64::try_from(fee.to_sat()).unwrap(), measured_fee);
	assert!(child.output.iter().all(|out| out.script_pubkey != destination));

	ctx.generate_blocks(6).await;
	let confirmed = canonical(&ctx, &parent);
	assert_eq!(canonical(&ctx, &child), confirmed, "the real sponsor child confirms with its immutable parent");
	let block = confirmed.hash;
	let payout = rpc.get_tx_out(&parent.compute_txid(), payout_index, Some(true)).unwrap().unwrap();
	assert_eq!(payout.value, sat(99_000));
	assert!(payout.confirmations >= 6);
	for (transaction, index, expected) in parent.output.iter().enumerate()
		.filter(|(_, output)| !output.is_p2a_fee_anchor())
		.map(|(index, output)| (&parent, index, output.value))
		.chain(child.output.iter().enumerate().map(|(index, output)| (&child, index, output.value)))
	{
		let output = rpc.get_tx_out(&transaction.compute_txid(), index as u32, Some(true)).unwrap().unwrap();
		assert_eq!(output.value, expected);
		assert!(output.confirmations >= 6);
	}
	let tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	wait_for_confirmation(&db, allocation, tip, block).await;
	let final_state = db.read(async |t| Ok(t.query_one("SELECT (SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts, \
		(SELECT COUNT(*) FROM expiry_entitlement d WHERE d.id=e.id) AS fences, \
		(SELECT COUNT(*) FROM expiry_allocation_coin c WHERE c.entitlement_id=e.id AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')) AS active, \
		(SELECT COUNT(*) FROM expiry_allocation_coin c WHERE c.entitlement_id=e.id) AS associations, \
		(SELECT COUNT(DISTINCT a.sweep_txid) FROM expiry_allocation_coin c JOIN expiry_allocation_state a ON a.id=c.allocation_id \
		 WHERE c.entitlement_id=e.id AND a.status='confirmed') AS payments, \
		(SELECT fee_sat FROM expiry_allocation_coin c WHERE c.entitlement_id=e.id AND c.allocation_id=$2) AS output_fee, \
		(SELECT sweep_tx FROM expiry_allocation_state WHERE id=$2) AS raw \
		FROM expiry_entitlement e WHERE e.id=$1", &[&entitlement,&allocation]).await?)).await.unwrap();
	assert_eq!(final_state.get::<_, String>("facts"), facts);
	assert_eq!(final_state.get::<_, i64>("fences"), 1);
	assert_eq!(final_state.get::<_, i64>("active"), 1);
	assert_eq!(final_state.get::<_, i64>("associations"), 1);
	assert_eq!(final_state.get::<_, i64>("payments"), 1);
	assert_eq!(final_state.get::<_, i64>("output_fee"), 1_000);
	assert_eq!(deserialize::<Transaction>(final_state.get("raw")).unwrap(), parent);
	// Exact selected cut: customer backing100k + independent fee capital100k.
	// The 1k output charge is operator capital, not an extra miner expense.
	assert_eq!(sat(200_000), sat(99_000) + sat(1_000)
		+ child.output.iter().map(|out| out.value).sum::<bitcoin::Amount>() + fee);
}
