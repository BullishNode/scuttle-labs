use std::time::Duration;

use ark::arkoor::{ArkoorDestination, package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse}};
use ark::lightning::PaymentHash;
use ark::VtxoPolicy;
use ark_testing::{sat, TestContext};
use bitcoin::consensus::deserialize;
use bitcoin::Transaction;
use bitcoin_ext::{BlockDelta, BlockHeight, TransactionExt};
use bitcoin_ext::bdk::TxBuilderExt;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use bark::onchain::{MakeCpfpFees, OnchainWalletTrait};
use server::database::Db;
use server_rpc::protos;

async fn payout(db: &Db, id: ark::VtxoId) -> Transaction {
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_opt(
				"
					SELECT a.sweep_tx
					FROM expiry_allocation a
					JOIN expiry_allocation_coin c ON c.allocation_id=a.id
					JOIN expiry_entitlement e ON e.id=c.entitlement_id
					WHERE e.vtxo_id=$1
					AND a.broadcast_at IS NOT NULL
				",
				&[&id.to_string()],
			).await?)).await.unwrap();
			if let Some(row) = row { return deserialize(row.get("sweep_tx")).unwrap(); }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("native watchman did not publish this sibling's payout")
}

/// Exercise the split API on a registered mixed tree, not manufactured
/// vtxo/transaction rows. The test requests the first split explicitly to cover
/// its SQL checks; both daemon progression and subsequent payouts are automatic.
#[tokio::test]
async fn expiry_requested_split_preserves_mixed_siblings_and_eventual_refund() {
	Box::pin(mixed_split(false, false, false)).await;
}

#[tokio::test]
async fn mempool_pinned_root_is_fee_bumped() {
	Box::pin(mixed_split(true, false, false)).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn split_frontier_survives_crash_before_successor_commit() {
	Box::pin(mixed_split(false, true, false)).await;
}

#[tokio::test]
async fn large_old_child_fee_is_preserved_when_bumping_pinned_root() {
	Box::pin(mixed_split(true, false, true)).await;
}

async fn mixed_split(pin: bool, crash_successor: bool, large_child: bool) {
	assert!(!crash_successor || !pin);
	assert!(!large_child || pin);
	let ctx = TestContext::new(if crash_successor { "server/split_frontier_successor_commit_crash" }
		else if large_child { "server/large_old_child_pinned_root" }
		else if pin { "server/mempool_pinned_root_is_fee_bumped" }
		else { "server/expiry_requested_mixed_split" }).await;
	let srv = ctx.captaind("server").no_vtxo_pool()
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24)).watchmand()
		.watchmand_cfg(|cfg| cfg.postgres.max_connections = 1).create().await;
	let wallet = ctx.bark("abandoned", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = wallet.client().await;
	let input = client.get_full_vtxo(ids[0]).await.unwrap();
	let destination = client.fallback_script();
	let key = client.get_vtxo_key(&input).await.unwrap();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let builder = ArkoorPackageBuilder::new_with_checkpoints([input.clone()], vec![
		ArkoorDestination { total_amount: sat(50_000), policy: VtxoPolicy::new_pubkey(key.public_key()) },
		ArkoorDestination { total_amount: sat(50_000), policy: VtxoPolicy::new_server_htlc_send(key.public_key(), PaymentHash::from([19; 32]), deadline) },
	]).unwrap().generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
	}).await.unwrap().into_inner();
	let package = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap()).unwrap();
	let checkpoint = package.signed_virtual_transactions().into_iter()
		.find(|tx| tx.input[0].previous_output == input.point()).unwrap();
	let leaves = package.build_signed_vtxos();
	let ordinary = leaves.iter().find(|v| matches!(v.policy(), VtxoPolicy::Pubkey(_))).unwrap().id();
	let htlc = leaves.iter().find(|v| matches!(v.policy(), VtxoPolicy::ServerHtlcSend(_))).unwrap().id();
	client.register_vtxo_transactions_with_server(&leaves).await.unwrap();
	let pin_sponsor = if pin {
		let mut sponsor = wallet.onchain_client().await;
		sponsor.sync(client.chain()).await.unwrap();
		Some(sponsor)
	} else { None };
	drop(client);
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	if pin { srv.watchmand().stop().await.unwrap(); }
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(input.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	if !pin { srv.watchmand().wait_for_sync_height(expired).await; }
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	let root = db.read(async |t| {
		let snapshot = t.backing_snapshot(&[ark::ServerVtxo::from(input.clone()).to_bare()],
			server::database::expiry_inventory::BackingSnapshotLimits { max_rows: 10_000, max_serialized_bytes: 10_000_000 }).await?;
		Ok(snapshot.regular.into_iter().find(|f| f.state.vtxo.point() == input.chain_anchor()).unwrap().state.vtxo)
	}).await.unwrap();
	if let Some(mut sponsor) = pin_sponsor {
		let prefix = db.read(async |tx| {
			let row = tx.query_one("SELECT t.signed_tx FROM vtxo v JOIN virtual_transaction t ON t.txid=v.oor_spent_txid \
				WHERE v.vtxo_id=$1", &[&root.id().to_string()]).await?;
			Ok(deserialize::<Transaction>(row.get("signed_tx"))?)
		}).await.unwrap();
		assert_eq!(prefix.input.len(), 1);
		assert_eq!(prefix.input[0].previous_output, root.point());
		assert!(!prefix.input[0].witness.is_empty());
		let target = srv.watchmand().config().fee_estimator.fallback_fee_rate_regular;
		let child = if large_child {
			assert_eq!(target, bitcoin::FeeRate::from_sat_per_vb(7).unwrap());
			large_own_output_fee_child(&mut sponsor, &prefix).await
		} else {
			sponsor.make_signed_p2a_cpfp(&prefix,
				MakeCpfpFees::Effective(bitcoin::FeeRate::from_sat_per_vb(1).unwrap())).await.unwrap()
		};
		sponsor.store_signed_p2a_cpfp(&child).await.unwrap();
		drop(sponsor);
		let core = ctx.bitcoind().sync_client();
		assert_eq!(core.submit_package(&[&prefix, &child]).unwrap().package_msg, "success");
		assert!(core.get_tx_out(&root.point().txid, root.point().vout, Some(true)).unwrap().is_none());
		let initial_fee = super::expiry_test::actual_fee(&ctx, &child).await;
		if large_child {
			assert!(child.vsize() < 1_000, "the real old TRUC child must fit Core policy");
			assert!(initial_fee < target * (prefix.weight() + child.weight()),
				"the old package must genuinely have a lower rate than the unchanged target");
		}
		assert!(db.read(async |tx| Ok(tx.query_one("SELECT onchain_spent_height FROM vtxo WHERE vtxo_id=$1",
			&[&root.id().to_string()]).await?.get::<_, Option<i32>>(0))).await.unwrap().is_none());
		srv.watchmand().start().await.unwrap();
		srv.watchmand().wait_for_sync_height(expired).await;
		srv.watchmand().trigger_sweep().await;
		let bumped = tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				if let Some((replacement, fee)) = ark_testing::onchain::observed_payout_fee_child(&ctx, &prefix) {
					if replacement.compute_txid() != child.compute_txid() && fee > initial_fee.to_sat() as i64 {
						break replacement;
					}
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the known pinned prefix did not receive its native watchman fee bump");
		assert_ne!(bumped.compute_txid(), child.compute_txid());
		if large_child {
			let effective_floor = MakeCpfpFees::Effective(target).package_fee(prefix.weight(), bumped.weight());
			assert!(child.weight() > bumped.weight());
			assert!(initial_fee > effective_floor,
				"the actual old fee must exceed the original Effective floor for this actual accepted smaller child");
			let replacement_fee = super::expiry_test::actual_fee(&ctx, &bumped).await;
			assert!(replacement_fee > initial_fee);
			assert!(u128::from(replacement_fee.to_sat()) * u128::from((prefix.weight() + child.weight()).to_wu())
				> u128::from(initial_fee.to_sat()) * u128::from((prefix.weight() + bumped.weight()).to_wu()));
			assert!(core.get_mempool_entry(&bumped.compute_txid()).is_ok(), "the replacement must actually be accepted by Core");
		}
		assert_eq!(ctx.bitcoind().await_transaction(prefix.compute_txid()).await, prefix);
		assert!(core.get_mempool_entry(&child.compute_txid()).is_err(), "the superseded fee child must not remain a second payment");
		assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT progress_txid FROM expiry_split_request WHERE vtxo_id=$1",
			&[&root.id().to_string()]).await?.get::<_, String>(0))).await.unwrap(), prefix.compute_txid().to_string(),
			"the mempool-null readiness path must request this exact signed progress");
	}
	tokio::time::timeout(Duration::from_secs(15), async {
		loop {
			let result = db.write(async |t| {
				let Some(view) = t.expiry_view().await? else { return Ok(false); };
				if !t.expiry_view_is_current(view).await? { return Ok(false); }
				let requested = t.request_expiry_split(view, &root).await?;
				if requested { t.record_expiry_hold(root.id(), "output_requires_split").await?; }
				Ok(requested)
			}).await.unwrap();
			if result { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual signed checkpoint paths should authorize this split");
	assert!(db.read(async |t| t.list_expiry_holds().await).await.unwrap().iter()
		.any(|hold| hold.vtxo_id == root.id() && hold.reason == "output_requires_split"));
	ctx.bitcoind().await_transaction(input.point().txid).await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&root.point().txid, root.point().vout, Some(true)).unwrap().is_none());
	// A mempool prefix makes gettxout unavailable before frontier confirmation.
	// Preserve that real transient reason until the exact prefix confirms.
	db.write(async |t| t.record_expiry_hold(root.id(), "backing_output_unavailable").await).await.unwrap();
	// Confirm the board exit, then allow the configured watchtower grace
	// period to pass. Waiting in wall-clock time cannot advance this policy.
	ctx.generate_blocks(1 + srv.watchmand().config().watchman.progress_grace_period.to_u32()).await;
	ctx.bitcoind().await_transaction(checkpoint.compute_txid()).await;
	let progressed = {
		#[cfg(target_os = "linux")]
		if crash_successor { crash_during_successor_registration(&ctx, &srv, &db, &checkpoint).await }
		else { ctx.generate_blocks(1).await }
		#[cfg(not(target_os = "linux"))]
		{ assert!(!crash_successor); ctx.generate_blocks(1).await }
	};
	srv.wait_for_sync_height(progressed).await;
	srv.watchmand().wait_for_sync_height(progressed).await;
	assert!(!db.read(async |t| t.list_expiry_holds().await).await.unwrap().iter()
		.any(|hold| hold.vtxo_id == root.id()), "a confirmed matching prefix resolves its old physical-parent split hold");
	for reason in ["output_requires_split", "backing_output_unavailable", "output_preparation_failed", "anchor_preparation_failed"] {
		db.write(async |t| t.record_expiry_hold(root.id(), reason).await).await.unwrap();
	}
	assert!(!db.read(async |t| t.list_expiry_holds().await).await.unwrap().iter()
		.any(|hold| hold.vtxo_id == root.id()), "late observations cannot recreate a completed prefix hold");
	let first = payout(&db, ordinary).await;
	assert_eq!(ctx.bitcoind().await_transaction(first.compute_txid()).await, first);
	let backing = first.input.iter().map(|i| ark::VtxoId::from(i.previous_output)).collect::<Vec<_>>();
	let reserved = db.write(async |t| t.lock_unallocated_watchman_inputs(&backing).await).await
		.expect_err("normal watchman progression must respect the actual payout's physical backing reservation");
	let refusal = format!("{reserved:#}");
	assert!(backing.iter().any(|id| refusal.contains(&format!("vtxo {id} is reserved for expiry recovery"))),
		"{refusal}");
	assert_eq!(first.output.iter().find(|o| o.script_pubkey == destination).unwrap().value, sat(49_000));
	let waiting = db.read(async |t| Ok(t.query_one("
		SELECT COUNT(*)
		FROM expiry_entitlement
		WHERE vtxo_id=$1
	", &[&htlc.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(waiting, 0, "ordinary payout must not invent a terminal HTLC resolution");
	assert!((ctx.bitcoind().get_block_count().await as u32) < deadline.to_u32());
	ctx.generate_blocks(6).await;
	let output_index = first.output.iter().position(|o| o.script_pubkey == destination).unwrap() as u32;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&first.compute_txid(), output_index, Some(true)).unwrap().unwrap().confirmations >= 6);

	// This registered send was never initiated. Once its refund deadline
	// passes, the retained checkpoint can pay it without a user signature or
	// a fabricated upstream off-chain Revoked state.
	let height = ctx.bitcoind().get_block_count().await as u32;
	let final_tip = ctx.generate_blocks(deadline.to_u32() + 1 - height).await;
	srv.wait_for_sync_height(final_tip).await;
	srv.watchmand().wait_for_sync_height(final_tip).await;
	let second = payout(&db, htlc).await;
	assert_ne!(first.compute_txid(), second.compute_txid());
	assert_eq!(ctx.bitcoind().await_transaction(second.compute_txid()).await, second);
	assert_eq!(second.output.iter().find(|o| o.script_pubkey == destination).unwrap().value, sat(50_000));
	assert_eq!(first.output.iter().filter(|o| o.script_pubkey == destination).map(|o| o.value).sum::<bitcoin::Amount>()
		+ second.output.iter().filter(|o| o.script_pubkey == destination).map(|o| o.value).sum::<bitcoin::Amount>(), sat(99_000),
		"both 50,000 sat siblings pay the 1,000 sat floor once at their original expiry height");
	let upstream = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, htlc).await).await.unwrap().unwrap();
	assert_eq!(upstream.htlc.resolution(), None);
	assert!(first.input.iter().all(|a| second.input.iter().all(|b| a.previous_output != b.previous_output)));
	ctx.generate_blocks(6).await;
	let output_index = second.output.iter().position(|o| o.script_pubkey == destination).unwrap() as u32;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&second.compute_txid(), output_index, Some(true)).unwrap().unwrap().confirmations >= 6);
}


/// Crash only the owned watchmand while a real nonfunding successor's first
/// frontier write is blocked. No business row or timestamp is manufactured.
#[cfg(target_os = "linux")]
async fn crash_during_successor_registration(
	ctx: &TestContext,
	srv: &ark_testing::Captaind,
	db: &Db,
	checkpoint: &Transaction,
) -> BlockHeight {
	const CLASS: i32 = 0x45583343;
	const OBJECT: i32 = 1;
	let txid = checkpoint.compute_txid();
	assert!(ctx.bitcoind().sync_client().get_mempool_entry(&txid).is_ok(),
		"the original signed checkpoint must still await its first confirmation");
	assert_eq!(checkpoint.input.len(), 1);
	let parent = ark::VtxoId::from(checkpoint.input[0].previous_output);
	let target = db.read(async |tx| Ok(tx.query_one("SELECT v.vtxo_id,v.amount,t.signed_tx,t.is_funding \
		FROM vtxo v JOIN virtual_transaction t ON t.txid=v.vtxo_txid \
		WHERE v.vtxo_txid=$1 AND v.frontier_at IS NULL AND v.onchain_spent_txid IS NULL \
		ORDER BY v.vtxo_id LIMIT 1", &[&txid.to_string()]).await?)).await.unwrap();
	assert!(!target.get::<_, bool>("is_funding"));
	assert_eq!(deserialize::<Transaction>(target.get("signed_tx")).unwrap(), *checkpoint);
	let id = target.get::<_, &str>("vtxo_id").parse::<ark::VtxoId>().unwrap();
	assert_eq!(id.to_point().txid, txid);
	assert_eq!(target.get::<_, i64>("amount") as u64, checkpoint.output[id.to_point().vout as usize].value.to_sat());
	let original_parent = db.read(async |tx| Ok(tx.query_one("SELECT frontier_at IS NOT NULL AS tracked, \
		onchain_spent_txid FROM vtxo WHERE vtxo_id=$1", &[&parent.to_string()]).await?)).await.unwrap();
	assert!(original_parent.get::<_, bool>("tracked"));
	assert!(original_parent.get::<_, Option<String>>("onchain_spent_txid").is_none());
	db.write(async |tx| {
		tx.batch_execute(&format!("CREATE FUNCTION ex3_hold_successor_frontier() RETURNS trigger AS $$
			BEGIN
			  IF NEW.vtxo_id='{id}' AND OLD.frontier_at IS NULL AND NEW.frontier_at IS NOT NULL THEN
			    PERFORM pg_advisory_xact_lock({CLASS},{OBJECT});
			  END IF;
			  RETURN NEW;
			END; $$ LANGUAGE plpgsql;
			CREATE TRIGGER ex3_hold_successor_frontier BEFORE UPDATE OF frontier_at ON vtxo
			FOR EACH ROW EXECUTE FUNCTION ex3_hold_successor_frontier();")).await?;
		Ok(())
	}).await.unwrap();
	let (old_child, writer, height) = db.write(async |tx| {
		tx.query_one("SELECT pg_advisory_xact_lock($1,$2)", &[&CLASS,&OBJECT]).await?;
		let blocker = tx.query_one("SELECT pg_backend_pid()", &[]).await?.get::<_, i32>(0);
		let height = ctx.generate_blocks(1).await;
		let writer = tokio::time::timeout(Duration::from_secs(15), async {
			loop {
				tx.query_one("SELECT pg_stat_clear_snapshot()", &[]).await?;
				let rows = tx.query("SELECT DISTINCT a.pid,a.backend_start FROM pg_stat_activity a JOIN pg_locks l ON l.pid=a.pid \
					WHERE a.datname=current_database() AND a.query LIKE '%SET frontier_at = NOW(), updated_at = NOW()%' \
					AND NOT l.granted AND l.locktype='advisory' AND l.classid::BIGINT=$1 AND l.objid::BIGINT=$2 \
					AND l.objsubid=2 AND $3=ANY(pg_blocking_pids(a.pid))",
					&[&i64::from(CLASS),&i64::from(OBJECT),&blocker]).await?;
				assert!(rows.len() <= 1);
				if let Some(row) = rows.first() {
					break Ok::<_, anyhow::Error>((row.get::<_, i32>(0),row.get::<_, chrono::DateTime<chrono::Utc>>(1)));
				}
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		}).await.expect("the actual successor registration did not reach its PG latch")?;
		let paused = srv.watchmand().pause_native().await?;
		let old_child = paused.identity().clone();
		let killed = srv.watchmand().crash_stop().await;
		drop(paused);
		killed?;
		Ok((old_child, writer, height))
	}).await.unwrap();
	// Release the test's advisory lock only after the client was killed/reaped.
	// The interrupted PG transaction must then abort on its closed connection.
	tokio::time::timeout(Duration::from_secs(15), async {
		loop {
			let alive = db.read(async |tx| Ok(tx.query_opt("SELECT 1 FROM pg_stat_activity \
				WHERE pid=$1 AND backend_start=$2", &[&writer.0,&writer.1]).await?.is_some())).await.unwrap();
			if !alive { break; }
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	}).await.expect("the exact interrupted backend must release its transaction before restart");
	let cut = db.read(async |tx| Ok(tx.query_one("SELECT p.onchain_spent_txid,p.onchain_spent_height, \
		c.frontier_at IS NOT NULL AS child_registered FROM vtxo p,vtxo c \
		WHERE p.vtxo_id=$1 AND c.vtxo_id=$2", &[&parent.to_string(),&id.to_string()]).await?)).await.unwrap();
	let parent_spent = cut.get::<_, Option<String>>("onchain_spent_txid");
	let parent_height = cut.get::<_, Option<i32>>("onchain_spent_height");
	let registered = cut.get::<_, bool>("child_registered");
	let hash = ctx.bitcoind().sync_client().get_block_hash(u64::from(height.to_u32())).unwrap();
	assert!(ctx.bitcoind().sync_client().get_block(&hash).unwrap().txdata.contains(checkpoint));
	tokio::fs::write(ctx.datadir.join("frontier-cold-cut.json"), serde_json::to_vec_pretty(&serde_json::json!({
		"parent":parent.to_string(),"successor":id.to_string(),"checkpoint":txid.to_string(),
		"height":height.to_u32(),"hash":hash.to_string(),"parent_spent_txid":parent_spent,
		"parent_spent_height":parent_height,"child_registered":registered,
		"blocked_pg_backend":writer,"killed_watchmand":old_child,
	})).unwrap()).await.unwrap();
	assert!(parent_spent.is_none() && parent_height.is_none(), "uncommitted successor registration must not leave a committed spent parent");
	assert!(!registered);
	db.write(async |tx| {
		tx.batch_execute("DROP TRIGGER ex3_hold_successor_frontier ON vtxo; DROP FUNCTION ex3_hold_successor_frontier();").await?;
		Ok(())
	}).await.unwrap();
	srv.watchmand().start().await.unwrap();
	assert_ne!(srv.watchmand().native_child().await.unwrap(), old_child);
	srv.watchmand().wait_for_sync_height(height).await;
	let recovered = db.read(async |tx| Ok(tx.query_one("SELECT p.onchain_spent_txid,p.onchain_spent_height, \
		c.frontier_at IS NOT NULL AS registered,c.confirmed_height FROM vtxo p,vtxo c \
		WHERE p.vtxo_id=$1 AND c.vtxo_id=$2", &[&parent.to_string(),&id.to_string()]).await?)).await.unwrap();
	assert_eq!(recovered.get::<_, Option<&str>>("onchain_spent_txid"), Some(txid.to_string().as_str()));
	assert_eq!(recovered.get::<_, Option<i32>>("onchain_spent_height"), Some(height.to_u32() as i32));
	assert!(recovered.get::<_, bool>("registered"));
	assert_eq!(recovered.get::<_, Option<i32>>("confirmed_height"), Some(height.to_u32() as i32));
	assert_eq!(ctx.bitcoind().get_block_count().await, u64::from(height.to_u32()));
	assert_eq!(ctx.bitcoind().sync_client().get_block_hash(u64::from(height.to_u32())).unwrap(), hash);
	height
}

/// A genuine low-rate, high-absolute-fee child using the original sponsor's
/// confirmed funds. Extra outputs all return to this same native wallet.
async fn large_own_output_fee_child(
	sponsor: &mut bark::onchain::OnchainWallet,
	parent: &Transaction,
) -> Transaction {
	let rate = bitcoin::FeeRate::from_sat_per_kwu(875); // 3.5 sat/vB, below the unchanged regular7.
	let (anchor, anchor_output) = parent.fee_anchor().unwrap();
	let mut outputs = Vec::new();
	for _ in 0..15 {
		let script = OnchainWalletTrait::address(sponsor).await.unwrap().script_pubkey();
		assert!(script.is_p2tr());
		assert!(OnchainWalletTrait::is_mine(sponsor, &script).await.unwrap());
		outputs.push((script.clone(), script.minimal_non_dust()));
	}
	let change = OnchainWalletTrait::address(sponsor).await.unwrap().script_pubkey();
	assert!(OnchainWalletTrait::is_mine(sponsor, &change).await.unwrap());
	let confirmed = sponsor.list_unspent().into_iter().filter(|coin| coin.chain_position.is_confirmed())
		.map(|coin| coin.outpoint).collect::<std::collections::HashSet<_>>();
	assert!(!confirmed.is_empty());
	let mut fee = rate * parent.weight();
	for _ in 0..3 {
		let psbt = {
			let mut builder = sponsor.build_tx();
			builder.only_witness_utxo().exclude_unconfirmed().version(3);
			builder.add_fee_anchor_spend(anchor, anchor_output);
			for (script, value) in &outputs { builder.add_recipient(script.clone(), *value); }
			builder.drain_to(change.clone()).fee_absolute(fee);
			builder.finish().unwrap()
		};
		let psbt = OnchainWalletTrait::sign_psbt(sponsor, psbt).await.unwrap();
		let actual_fee = psbt.fee().unwrap();
		let child = psbt.extract_tx().unwrap();
		assert_eq!(child.input.iter().filter(|input| input.previous_output == anchor).count(), 1);
		assert!(child.input.iter().filter(|input| input.previous_output != anchor)
			.all(|input| confirmed.contains(&input.previous_output)));
		assert!(child.vsize() < 1_000);
		for output in &child.output {
			assert!(output.value >= output.script_pubkey.minimal_non_dust());
			assert!(OnchainWalletTrait::is_mine(sponsor, &output.script_pubkey).await.unwrap());
		}
		let required = rate * (parent.weight() + child.weight());
		if actual_fee >= required { return child; }
		fee = required;
	}
	panic!("native signed fee-child weight did not stabilize");
}

/// A key with no fallback registration keeps its backing while a registered
/// sibling under the same anchor receives its ordinary expiry payout.
#[tokio::test]
async fn expiry_unregistered_owner_does_not_block_registered_sibling() {
	held_sibling(false).await;
}

#[tokio::test]
async fn one_bad_terminal_holds_only_itself() {
	held_sibling(true).await;
}

async fn held_sibling(invalid_terminal: bool) {
	let ctx = TestContext::new(if invalid_terminal { "server/one_bad_terminal_holds_only_itself" }
		else { "server/expiry_unregistered_owner_with_registered_sibling" }).await;
	let srv = ctx.captaind("server").no_vtxo_pool()
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24)).watchmand().create().await;
	let wallet = ctx.bark("sender", &srv).funded(sat(200_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = wallet.client().await;
	client.stop_daemon_wait().await.unwrap();
	let source = client.get_full_vtxo(ids[0]).await.unwrap();
	let destination = client.fallback_script();
	let key = client.get_vtxo_key(&source).await.unwrap();
	let unknown_pair = bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[91; 32]).unwrap();
	let unknown = unknown_pair.public_key();
	super::test_owner_link(&srv, &unknown_pair).await;
	assert_ne!(key.public_key(), unknown);
	let builder = ArkoorPackageBuilder::new_with_checkpoints([source.clone()], vec![
		ArkoorDestination { total_amount: sat(90_000), policy: VtxoPolicy::new_pubkey(key.public_key()) },
		ArkoorDestination { total_amount: sat(10_000), policy: VtxoPolicy::new_pubkey(unknown) },
	]).unwrap().generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = rpc.request_arkoor_cosign(super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await)
		.await.unwrap().into_inner();
	let signed = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap()).unwrap();
	let checkpoint = signed.signed_virtual_transactions().into_iter()
		.find(|tx| tx.input[0].previous_output == source.point()).unwrap();
	let leaves = signed.build_signed_vtxos();
	assert_eq!(leaves.len(), 2);
	let ready = &leaves[0];
	let held = &leaves[1];
	assert_eq!(ready.policy(), &VtxoPolicy::new_pubkey(key.public_key()));
	assert_eq!(held.policy(), &VtxoPolicy::new_pubkey(unknown));
	assert_eq!(ready.amount(), sat(90_000));
	assert_eq!(held.amount(), sat(10_000));
	assert_eq!(ready.chain_anchor(), held.chain_anchor());
	assert_eq!(ready.expiry_height(), held.expiry_height());
	let held_checkpoint = bitcoin::OutPoint::new(checkpoint.compute_txid(), 1);
	assert_eq!(checkpoint.output[1].value, held.amount());
	client.register_vtxo_transactions_with_server(&leaves).await.unwrap();
	let funding = ctx.bitcoind().await_transaction(source.chain_anchor().txid).await;
	for leaf in &leaves { leaf.validate(&funding).unwrap(); }
	drop(client);

	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	// Deliberately corrupt attribution evidence after genuine signed admission.
	assert!(matches!(db.read(async |tx| tx.expiry_owner(unknown).await).await.unwrap(), server::database::expiry_attribution::Attribution::Owned { mailbox_pk, .. } if mailbox_pk == unknown));
	db.write(async |tx| {
		if invalid_terminal {
			// The signed path and real owner remain valid. A malformed committed
			// customer key makes only this terminal's ownership reader fail.
			tx.execute("INSERT INTO expiry_entitlement(vtxo_id,gross_sat,irreversible_facts) VALUES($1,$2,$3::TEXT::JSONB)",
				&[&held.id().to_string(), &10_000i64, &r#"{"kind":"pubkey","customer_key":"not-a-public-key"}"#]).await?;
			tx.execute("UPDATE vtxo SET spend_state='spent',updated_at=NOW() WHERE vtxo_id=$1", &[&held.id().to_string()]).await?;
		} else {
			tx.execute("DELETE FROM owner_key WHERE user_pubkey=$1", &[&unknown.to_string()]).await?;
		}
		Ok(())
	}).await.unwrap();
	let height = ctx.bitcoind().get_block_count().await as u32;
	let scan_from = u64::from(height) + 1;
	assert!(height < source.expiry_height().to_u32());
	let expired = ctx.generate_blocks(source.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let core = ctx.bitcoind().sync_client();
	let parent = tokio::time::timeout(Duration::from_secs(45), async {
		for _ in 0..24 {
			let row = db.read(async |tx| Ok(tx.query_opt("SELECT a.sweep_tx FROM expiry_allocation a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND c.active AND a.broadcast_at IS NOT NULL",
				&[&ready.id().to_string()],
			).await?)).await.unwrap();
			if let Some(row) = row { return deserialize::<Transaction>(row.get("sweep_tx")).unwrap(); }
			srv.watchmand().trigger_sweep().await;
			tokio::time::sleep(Duration::from_millis(750)).await;
			let tip = ctx.generate_blocks(1).await;
			srv.wait_for_sync_height(tip).await;
			srv.watchmand().wait_for_sync_height(tip).await;
		}
		panic!("registered sibling did not pay within 24 progress blocks");
	}).await.expect("registered sibling payout must not wait for an unregistered key");
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	let outputs = parent.output.iter().enumerate()
		.filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(outputs.len(), 1);
	assert_eq!(outputs[0].1.value, sat(89_000));
	let tip = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(tip).await;
	srv.watchmand().wait_for_sync_height(tip).await;
	let paid = core.get_tx_out(&parent.compute_txid(), outputs[0].0 as u32, Some(true)).unwrap().unwrap();
	assert!(paid.confirmations >= 6);
	assert_eq!(paid.value, sat(89_000));
	let state = db.read(async |tx| tx.expiry_owner(unknown).await).await.unwrap();
	if invalid_terminal {
		assert!(matches!(state, server::database::expiry_attribution::Attribution::Owned { mailbox_pk, .. } if mailbox_pk == unknown));
		assert!(db.read(async |tx| Ok(tx.query_one("SELECT e.irreversible_facts->>'customer_key'='not-a-public-key' \
			AND EXISTS(SELECT 1 FROM expiry_hold h WHERE h.vtxo_id=e.vtxo_id AND h.reason='terminal_ownership_invalid') \
			AND NOT EXISTS(SELECT 1 FROM expiry_allocation_coin c WHERE c.entitlement_id=e.id) \
			FROM expiry_entitlement e WHERE e.vtxo_id=$1", &[&held.id().to_string()],
		).await?.get::<_, bool>(0))).await.unwrap());
	} else {
		assert_eq!(state, server::database::expiry_attribution::Attribution::Unresolved);
		assert!(db.read(async |tx| Ok(tx.query_one("SELECT \
			NOT EXISTS(SELECT 1 FROM owner_key WHERE user_pubkey=$1) \
			AND NOT EXISTS(SELECT 1 FROM expiry_entitlement WHERE vtxo_id=$2)",
			&[&unknown.to_string(), &held.id().to_string()],
		).await?.get::<_, bool>(0))).await.unwrap());
	}
	let rows = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id,c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat \
		FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
		WHERE e.vtxo_id=ANY($1) AND c.active", &[&leaves.iter().map(|leaf| leaf.id().to_string()).collect::<Vec<_>>()],
	).await?)).await.unwrap();
	assert_eq!(rows.len(), 1);
	assert_eq!(rows[0].get::<_, String>("vtxo_id"), ready.id().to_string());
	for (field, amount) in [("gross_sat", 90_000), ("fee_sat", 1_000), ("payout_sat", 89_000), ("forfeited_sat", 0)] {
		assert_eq!(rows[0].get::<_, i64>(field), amount);
	}
	let mut unspent = 0;
	for (point, expected) in [(held_checkpoint, checkpoint.output[1].clone()), (held.point(), held.txout())] {
		if let Some(output) = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap() {
			assert!(output.confirmations >= 6);
			assert_eq!(output.value, held.amount());
			assert_eq!(output.script_pub_key.script().unwrap(), expected.script_pubkey);
			unspent += 1;
		}
	}
	assert_eq!(unspent, 1, "the unregistered owner's branch must retain exactly one unspent backing output");
	let mut payments = Vec::new();
	for height in scan_from..=u64::from(tip.to_u32()) {
		let block = core.get_block(&core.get_block_hash(height).unwrap()).unwrap();
		for transaction in block.txdata {
			for (index, output) in transaction.output.iter().enumerate() {
				if output.script_pubkey == destination {
					assert_eq!(output.value, sat(89_000));
					payments.push(bitcoin::OutPoint::new(transaction.compute_txid(), index as u32));
				}
			}
		}
	}
	assert_eq!(payments, vec![bitcoin::OutPoint::new(parent.compute_txid(), outputs[0].0 as u32)]);
	assert_eq!(sat(89_000) + sat(1_000) + held.amount(), source.amount());
}
