use std::time::Duration;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::body::Bytes;
use axum::http::{HeaderMap, header};
use axum::routing::post;
use axum::Router;

use ark::ProtocolEncoding;

use bitcoincore_rpc::RpcApi;

use bark::{OpenWalletArgs, Wallet, WalletNotification, WalletSeed};
use bark::persist::{BarkPersister, sqlite::SqliteClient};
use bark::exit::{ExitError, ExitState};
use bark::movement::{Movement, MovementId, MovementStatus};
use bitcoin_ext::BlockHeight;
use bitcoin_ext::rpc::BitcoinRpcExt;
use futures::StreamExt;
use server_log::ClaimBroadcast;

use ark_testing::{TestContext, btc, sat};
use ark_testing::constants::ROUND_CONFIRMATIONS;
use ark_testing::util::FutureExt;

// Check every allocation generation and root descendant, including spent payouts.
// These fallback-only cases separately reject terminal user claims.

fn chain_tip(ctx: &TestContext) -> BlockHeight {
	let count = ctx.bitcoind().sync_client().get_block_count().expect("block count should succeed");
	BlockHeight::new(count as u32)
}

/// How many blocks past the current tip `height` is, zero if the tip is already there.
fn blocks_until(ctx: &TestContext, height: BlockHeight) -> u32 {
	height.checked_blocks_since(chain_tip(ctx)).unwrap_or(0)
}

/// A VTXO consumed outside the exit flow (e.g. spent via arkoor) has no exit entry; the
/// estimate must still reject it as already spent.
#[tokio::test]
async fn exit_estimate_rejects_spent_vtxo() {
	let ctx = TestContext::new("bark_sdk/exit_estimate_rejects_spent_vtxo").await;
	let srv = ctx.captaind("server").create().await;

	let sender = ctx.bark_sdk("bark", &srv).boarded(sat(400_000)).create().await;
	let receiver = ctx.bark_sdk("bark2", &srv).create().await;

	let [boarded] = sender.vtxos().await.expect("list vtxos")
		.try_into().expect("expected exactly one boarded vtxo");
	let spent_id = boarded.vtxo.id();

	let address = receiver.new_address().await.expect("new address");
	sender.send_arkoor_payment(&address, sat(100_000)).await.expect("arkoor send");

	let err = sender.estimate_emergency_exit_fee(&[spent_id], None, None, None).await
		.expect_err("estimating a spent VTXO must fail");
	assert_eq!(err, ExitError::VtxoAlreadySpent { vtxo: spent_id });
}

/// An invalid fee margin is a typed error, not a panic — including one that scales the fee out
/// of range.
#[tokio::test]
async fn exit_estimate_rejects_invalid_fee_margin() {
	let ctx = TestContext::new("bark_sdk/exit_estimate_rejects_invalid_fee_margin").await;
	let srv = ctx.captaind("server").create().await;
	let wallet = ctx.bark_sdk("bark", &srv).boarded(sat(400_000)).create().await;
	let [vtxo] = wallet.vtxos().await.expect("list vtxos")
		.try_into().expect("expected exactly one boarded vtxo");
	let vtxo_id = vtxo.vtxo.id();

	for margin in [f64::NAN, f64::INFINITY, -0.5, f64::MAX] {
		let err = wallet.estimate_emergency_exit_fee(&[vtxo_id], None, None, Some(margin)).await
			.expect_err("an invalid fee margin must be rejected");
		assert!(matches!(err, ExitError::InvalidFeeMargin { .. }), "{:?}", err);
	}
}

/// A native customer payout that spends the expired funding anchor only ends
/// an exit after deep confirmation, then discharges the coin without reminting it.
#[tokio::test]
async fn watchman_sweeps_funding_tx_during_exit() {
	let ctx = TestContext::new("bark_sdk/watchman_sweeps_funding_tx_during_exit").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let wm = srv.watchmand();
	let mut log_claim = wm.subscribe_log::<ClaimBroadcast>();

	// fund the watchman so it can pay CPFP fees for anything it broadcasts
	ctx.bitcoind().fund_addr(wm.wait_wallet_address().await, sat(1_000_000)).await;

	let wallet = ctx.bark_sdk("bark", &srv)
		.cfg(|c| c.daemon_manual_sync = true)
		.funded(sat(1_000_000))
		.boarded(sat(400_000))
		.create().await;

	let vtxos = wallet.vtxos().await.expect("listing vtxos");
	assert_eq!(vtxos.len(), 1, "the wallet should hold exactly one board vtxo");
	let vtxo = vtxos[0].id();
	let amount = vtxos[0].amount();
	let expiry = vtxos[0].vtxo.expiry_height();
	// The exit hangs off this on-chain output, which is what the watchman sweeps.
	let anchor = vtxos[0].vtxo.chain_anchor();

	let owners = [(&wallet, vtxo, amount)];

	// Capture the registered owners before the absent-client interval.
	let first_height = ctx.bitcoind().get_block_count().await;
	let db = server::database::Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	let operator_script = {
		let config = wm.config();
		config.sweep_address.clone().unwrap().require_network(config.network).unwrap().script_pubkey()
	};
	let mut source_path = std::collections::BTreeSet::from([anchor]);
	let mut expected = std::collections::BTreeMap::new();
	for (owner, id, gross) in owners {
		let full = owner.get_full_vtxo(id).await.unwrap();
		source_path.extend(full.transactions().map(|step|
			bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32)));
		let destination = owner.fallback_script();
		let key = owner.get_vtxo_key(id).await.unwrap();
		assert!(expected.insert(id.to_string(), (gross, destination,
			owner.mailbox_identifier().as_pubkey().to_string(), key.public_key().to_string())).is_none());
	}
	assert_eq!(expected.values().map(|entry| entry.0).sum::<bitcoin::Amount>(), sat(400_000));

	// The client is away, so the watchman sweeps the anchor before the exit starts.
	let tip = ctx.generate_blocks(blocks_until(&ctx, expiry) + 1).await;
	wm.wait_for_sync_height(tip).await;
	assert!(tip >= expiry, "the vtxo should be expired, tip {} expiry {}", tip, expiry);

	// Wait for a broadcast native customer parent, never inject a financial decision.
	let parent = async {
		'found: loop {
			wm.trigger_sweep().await;
			let rows = db.read(async |tx| Ok(tx.query("SELECT DISTINCT a.sweep_tx FROM expiry_allocation a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND c.disposition='customer' AND a.broadcast_at IS NOT NULL",
				&[&vtxo.to_string()],
			).await?)).await.unwrap();
			for row in rows {
				let parent = bitcoin::consensus::deserialize::<bitcoin::Transaction>(row.get("sweep_tx")).unwrap();
				if ctx.bitcoind().sync_client().custom_get_raw_transaction_info(parent.compute_txid(), None)
					.unwrap().is_some() { break 'found parent; }
			}
			tokio::time::sleep(Duration::from_millis(200)).await;
		}
	}.wait_millis(30000).await;

	assert!(parent.input.iter().any(|input| input.previous_output == anchor));
	let chain = vec![parent.compute_txid()];

	wallet.exit_mgr().start_exit_for_entire_wallet().await.expect("starting the exit");

	let before = ctx.bitcoind().sync_client().custom_get_raw_transaction_info(parent.compute_txid(), None)
		.unwrap().unwrap().confirmations.unwrap_or(0);
	assert!(before <= 1, "the native parent must be observed before deep confirmation");
	if before == 0 { ctx.generate_blocks(1).await; }
	wallet.chain().invalidate_caches().await;
	let mut shallow = None;
	for _ in 0..3 {
		wallet.sync_onchain().await.expect("onchain sync");
		let _ = wallet.progress_exits().await;
		shallow = wallet.exit_mgr().get_exit_vtxo(vtxo).await.map(|exit| exit.state().clone());
	}
	assert!(matches!(shallow, Some(ExitState::Processing(_))),
		"one-deep customer payout must not end the exit: {:?}", shallow);
	wallet.sync_fallback_payouts().await.unwrap();
	assert_eq!(wallet.get_vtxo_by_id(vtxo).await.unwrap().state, bark::vtxo::VtxoState::Locked { holder: None });
	assert_eq!(wallet.get_vtxo_by_id(vtxo).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	let registered = wallet.fallback_script();
	let payout_index = parent.output.iter().position(|output| output.script_pubkey == registered).unwrap() as u32;
	assert_eq!(parent.output[payout_index as usize].script_pubkey, registered);
	assert!(before < 6, "one confirmation cannot discharge the registered payout");

	let shallow_history = wallet.history().await.unwrap();
	assert!(!shallow_history.iter().any(|movement| movement.subsystem.name == "abandon.fallback_payout"
		&& movement.status == bark::movement::MovementStatus::Successful));

	// Give the client every chance to finish the exit and take the money.
	let mut claimable = false;
	let mut state = None;
	for _ in 0..15 {
		wallet.sync_onchain().await.expect("onchain sync");
		wallet.chain().invalidate_caches().await;
		// The broadcast cannot succeed once the output is gone, which is the point.
		let _ = wallet.progress_exits().await;
		state = wallet.exit_mgr().get_exit_vtxo(vtxo).await.map(|e| e.state().clone());
		claimable |= matches!(state, Some(ExitState::Claimable(_) | ExitState::Claimed(_)));
		ctx.generate_blocks(1).await;
	}

	assert!(matches!(state, Some(ExitState::VtxoSwept(_))),
		"exit of a swept vtxo must terminate as VtxoSwept, was {:?}", state,
	);
	// A swept output can never be claimed, so the exit must never have offered it.
	assert!(!claimable, "the exit of a swept vtxo must never become claimable");

	// Observe the real native confirmation projection before consuming its public receipt.
	wm.trigger_sweep().await;
	async {
		loop {
			let paid = db.read(async |tx| Ok(tx.query("SELECT DISTINCT e.vtxo_id FROM expiry_allocation_coin c \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id JOIN vtxo v ON v.vtxo_id=e.vtxo_id \
				JOIN expiry_allocation a ON a.id=c.allocation_id \
				WHERE v.anchor_point=$1 AND a.status='confirmed' \
				AND a.confirmed_height IS NOT NULL AND a.confirmed_hash IS NOT NULL \
				AND a.sweep_txid IS NOT NULL", &[&anchor.to_string()],
			).await?)).await.unwrap();
			if paid.len() == expected.len() { break; }
			wm.trigger_sweep().await;
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}.wait_millis(15000).await;

	let mut paid_points = std::collections::BTreeMap::new();
	let mut total_fees = sat(0);
	for (owner, id, gross) in owners {
		let fee = sat(gross.to_sat().div_ceil(100).max(1_000));
		total_fees += fee;
		owner.chain().invalidate_caches().await;
		owner.sync_fallback_payouts().await.unwrap();
		let expected_script = &expected[&id.to_string()].1;
		let signed = &parent;
		let index = signed.output.iter().position(|output|
			output.script_pubkey == *expected_script && output.value == gross - fee).unwrap() as u32;
		let point = bitcoin::OutPoint::new(signed.compute_txid(), index);
		assert_eq!(signed.output[index as usize].value, gross - fee);

		let info = ctx.bitcoind().sync_client().custom_get_raw_transaction_info(point.txid, None).unwrap().unwrap();
		assert!(info.confirmations.unwrap_or(0) >= 6);
		let block = info.blockhash.unwrap();
		let header = ctx.bitcoind().sync_client().get_block_header_info(&block).unwrap();
		assert_eq!(ctx.bitcoind().sync_client().get_block_hash(header.height as u64).unwrap(), block);
		assert_eq!(ctx.bitcoind().await_transaction(point.txid).await, *signed);
		let output = ctx.bitcoind().sync_client().get_tx_out(&point.txid, index, Some(true)).unwrap().unwrap();
		assert!(output.confirmations >= 6);
		assert_eq!(output.value, gross - fee);
		assert_eq!(output.script_pub_key.script().unwrap(), *expected_script);
		assert!(paid_points.insert(expected_script.clone(), (point, gross - fee)).is_none());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().state, bark::vtxo::VtxoState::Spent);
		assert!(owner.vtxos().await.unwrap().is_empty());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
		let before_refresh = owner.history().await.unwrap();
		assert!(!before_refresh.iter().any(|movement| movement.subsystem.name == "scuttle.fallback"),
			"Bitcoin settlement must not synthesize a second Ark movement");
		assert_eq!(gross, output.value + fee, "the original Core output and fee still account for all principal");

		assert!(owner.refresh_vtxos([id]).await.unwrap().is_none(), "paid principal must not enter another round");
		owner.sync_fallback_payouts().await.unwrap();
		assert_eq!(owner.history().await.unwrap(), before_refresh, "refresh and receipt replay must not debit or mint again");
	}
	assert_eq!(paid_points.values().map(|entry| entry.1).sum::<bitcoin::Amount>()
		+ total_fees, sat(400_000));

	// Repeat after another real rebuilt tip. Include all generations and all root descendants,
	// not just the selected active receipt, so an extra owner/operator allocation cannot hide.
	let final_height = ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	wm.wait_for_sync_height(final_height).await;
	wm.trigger_sweep().await;
	let final_hash = ctx.bitcoind().sync_client().get_block_hash(final_height.to_u32() as u64).unwrap();
	async {
		loop {
			let rebuilt = db.read(async |tx| {
				let Some(view) = tx.expiry_view().await? else { return Ok(false); };
				Ok(view.processed.height == final_height && view.processed.hash == final_hash
					&& tx.expiry_view_is_current(view).await?)
			}).await.unwrap();
			if rebuilt { break; }
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}.wait_millis(15000).await;
	assert_payouts_once(&ctx, &db, anchor, &expected, &paid_points, &source_path,
		&operator_script, first_height).await;
	while let Ok(claim) = log_claim.try_recv() {
		assert!(!claim.vtxo_ids.iter().any(|id| id.to_point() == anchor || chain.contains(&id.to_point().txid)),
			"the old operator-only claim path must not consume this customer backing");
	}
	for (owner, id, _) in owners {
		owner.chain().invalidate_caches().await;
		owner.sync_fallback_payouts().await.unwrap();
		assert!(owner.vtxos().await.unwrap().is_empty());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().state, bark::vtxo::VtxoState::Spent);
		assert_eq!(owner.history().await.unwrap().iter().filter(|movement|
			movement.subsystem.name == "abandon.fallback_payout").count(), 0);
	}
}

/// Race a partial exit chain against the native registered-customer
/// payout path; both same-root owners must receive their exact net principal once.
#[tokio::test]
async fn watchman_sweeps_vtxo_chain_during_exit() {
	let ctx = TestContext::new("bark_sdk/watchman_sweeps_vtxo_chain_during_exit").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let wm = srv.watchmand();
	let mut log_claim = wm.subscribe_log::<ClaimBroadcast>();

	// fund the watchman so it can pay CPFP fees for anything it broadcasts
	ctx.bitcoind().fund_addr(wm.wait_wallet_address().await, sat(1_000_000)).await;

	// the receiver needs a vtxo with a chain under it, so the sender sends it an arkoor.
	let sender = ctx.bark_sdk("bark1", &srv)
		.cfg(|c| {
			c.daemon_manual_sync = true;
			// Bound this race to two physical coins and two registered-owner outputs.
			c.change_vtxo_split_factor = 1;
		})
		.funded(sat(1_000_000))
		.boarded(sat(400_000))
		.create().await;
	let receiver = ctx.bark_sdk("bark2", &srv)
		.cfg(|c| c.daemon_manual_sync = true)
		.funded(sat(1_000_000))
		.create().await;
	let address = receiver.new_address().await.expect("new address");
	sender.send_arkoor_payment(&address, sat(100_000)).await.expect("arkoor send");
	// The receiver only learns about the arkoor once it drains its mailbox.
	receiver.maintenance().await.expect("receiver maintenance");

	let vtxos = receiver.vtxos().await.expect("listing vtxos");
	assert_eq!(vtxos.len(), 1, "the receiver should hold exactly the received vtxo");
	let vtxo = vtxos[0].id();
	let amount = vtxos[0].amount();
	let change = sender.vtxos().await.unwrap();
	assert_eq!(change.len(), 1);
	assert_eq!(change[0].amount(), sat(300_000));
	assert_eq!(change[0].vtxo.chain_anchor(), vtxos[0].vtxo.chain_anchor());
	assert_eq!(amount, sat(100_000));
	let owners = [(&sender, change[0].id(), change[0].amount()), (&receiver, vtxos[0].id(), amount)];
	let expiry = vtxos[0].vtxo.expiry_height();
	let anchor = vtxos[0].vtxo.chain_anchor();


	// Capture the registered owners before the absent-client interval.
	let first_height = ctx.bitcoind().get_block_count().await;
	let db = server::database::Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	let operator_script = {
		let config = wm.config();
		config.sweep_address.clone().unwrap().require_network(config.network).unwrap().script_pubkey()
	};
	let mut source_path = std::collections::BTreeSet::from([anchor]);
	let mut expected = std::collections::BTreeMap::new();
	for (owner, id, gross) in owners {
		let full = owner.get_full_vtxo(id).await.unwrap();
		source_path.extend(full.transactions().map(|step|
			bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32)));
		let destination = owner.fallback_script();
		let key = owner.get_vtxo_key(id).await.unwrap();
		assert!(expected.insert(id.to_string(), (gross, destination,
			owner.mailbox_identifier().as_pubkey().to_string(), key.public_key().to_string())).is_none());
	}
	assert_eq!(expected.values().map(|entry| entry.0).sum::<bitcoin::Amount>(), sat(400_000));

	// Start the exit one block before expiry so that only the top of the chain
	// can confirm in time. Each exit tx is broadcast only once its parent
	// confirms, so one progress plus one block leaves the chain confirmed exactly
	// one tx deep with the rest unbroadcast.
	let tip = ctx.generate_blocks(blocks_until(&ctx, expiry) - 1).await;
	assert!(tip < expiry, "should not have expired the vtxo yet, tip {}", tip);
	receiver.exit_mgr().start_exit_for_entire_wallet().await.expect("starting the exit");
	receiver.progress_exits().await.expect("progressing the exit");
	ctx.generate_blocks(1).await;

	// Record the chain while the exit is live: a finished exit no longer lists its txs.
	let exit = receiver.exit_mgr().get_exit_vtxo(vtxo).await
		.expect("the exit we started should be tracked");
	let chain = exit.txids().expect("a live exit lists its txs").to_vec();
	let confirmed = chain.iter().filter(|txid| {
		let status = ctx.bitcoind().sync_client().tx_status(**txid)
			.expect("tx status should succeed");
		status.is_confirmed()
	}).count();
	assert!(confirmed > 0 && confirmed < chain.len(),
		"the exit chain should be partially on chain, {}/{} txs confirmed",
		confirmed, chain.len(),
	);

	// Expiring the vtxo unlocks the sweep, the rest of the chain still unbroadcast. The watchman
	// won't act on the confirmed top of the chain until `vtxo_exit_delta` has passed since the
	// block it confirmed in, so clear that too.
	let sweepable = expiry + srv.config().vtxo_exit_delta;
	let tip = ctx.generate_blocks(blocks_until(&ctx, sweepable) + 1).await;
	wm.wait_for_sync_height(tip).await;
	assert!(tip >= expiry, "the vtxo should be expired, tip {} expiry {}", tip, expiry);

	// Wait for a broadcast native customer parent, never inject a financial decision.
	let parent = async {
		let mut advances = 0;
		'found: loop {
			wm.trigger_sweep().await;
			let rows = db.read(async |tx| Ok(tx.query("SELECT DISTINCT a.sweep_tx FROM expiry_allocation a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND c.disposition='customer' AND a.broadcast_at IS NOT NULL",
				&[&vtxo.to_string()],
			).await?)).await.unwrap();
			for row in rows {
				let parent = bitcoin::consensus::deserialize::<bitcoin::Transaction>(row.get("sweep_tx")).unwrap();
				if ctx.bitcoind().sync_client().custom_get_raw_transaction_info(parent.compute_txid(), None)
					.unwrap().is_some() { break 'found parent; }
			}
			tokio::time::sleep(Duration::from_millis(200)).await;
			assert!(advances < 15, "native partial-chain payout did not become broadcast within 15 mined steps");
			advances += 1;
			let tip = ctx.generate_blocks(1).await;
			wm.wait_for_sync_height(tip).await;
		}
	}.wait_millis(30000).await;

	assert!(parent.input.iter().any(|input| input.previous_output == anchor || chain.contains(&input.previous_output.txid)),
		"the real customer parent must break the observed partial exit chain");
	let before = ctx.bitcoind().sync_client().custom_get_raw_transaction_info(parent.compute_txid(), None)
		.unwrap().unwrap().confirmations.unwrap_or(0);
	assert!(before <= 1, "the native parent must be observed before deep confirmation");
	if before == 0 { ctx.generate_blocks(1).await; }
	receiver.chain().invalidate_caches().await;
	let mut shallow = None;
	for _ in 0..3 {
		receiver.sync_onchain().await.expect("onchain sync");
		let _ = receiver.progress_exits().await;
		shallow = receiver.exit_mgr().get_exit_vtxo(vtxo).await.map(|exit| exit.state().clone());
	}
	assert!(matches!(shallow, Some(ExitState::Processing(_))),
		"one-deep customer payout must not end the exit: {:?}", shallow);
	receiver.sync_fallback_payouts().await.unwrap();
	assert_eq!(receiver.get_vtxo_by_id(vtxo).await.unwrap().state, bark::vtxo::VtxoState::Locked { holder: None });
	assert_eq!(receiver.get_vtxo_by_id(vtxo).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
	let registered = receiver.fallback_script();
	let payout_index = parent.output.iter().position(|output| output.script_pubkey == registered).unwrap() as u32;
	assert_eq!(parent.output[payout_index as usize].script_pubkey, registered);
	assert!(before < 6, "one confirmation cannot discharge the registered payout");

	let shallow_history = receiver.history().await.unwrap();
	assert!(!shallow_history.iter().any(|movement| movement.subsystem.name == "abandon.fallback_payout"
		&& movement.status == bark::movement::MovementStatus::Successful));

	// Give the client every chance to finish the exit and take the money.
	let mut claimable = false;
	let mut state = None;
	for _ in 0..15 {
		receiver.sync_onchain().await.expect("onchain sync");
		receiver.chain().invalidate_caches().await;
		// The broadcast cannot succeed once the output is gone, which is the point.
		let _ = receiver.progress_exits().await;
		state = receiver.exit_mgr().get_exit_vtxo(vtxo).await.map(|e| e.state().clone());
		claimable |= matches!(state, Some(ExitState::Claimable(_) | ExitState::Claimed(_)));
		ctx.generate_blocks(1).await;
	}

	assert!(matches!(state, Some(ExitState::VtxoSwept(_))),
		"exit whose chain was swept must terminate as VtxoSwept, was {:?}", state,
	);
	// A swept output can never be claimed, so the exit must never have offered it.
	assert!(!claimable, "the exit of a swept vtxo must never become claimable");

	// Observe the real native confirmation projection before consuming its public receipt.
	wm.trigger_sweep().await;
	async {
		loop {
			let paid = db.read(async |tx| Ok(tx.query("SELECT DISTINCT e.vtxo_id FROM expiry_allocation_coin c \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id JOIN vtxo v ON v.vtxo_id=e.vtxo_id \
				JOIN expiry_allocation a ON a.id=c.allocation_id \
				WHERE v.anchor_point=$1 AND a.status='confirmed' \
				AND a.confirmed_height IS NOT NULL AND a.confirmed_hash IS NOT NULL \
				AND a.sweep_txid IS NOT NULL", &[&anchor.to_string()],
			).await?)).await.unwrap();
			if paid.len() == expected.len() { break; }
			wm.trigger_sweep().await;
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}.wait_millis(15000).await;

	let mut paid_points = std::collections::BTreeMap::new();
	let mut total_fees = sat(0);
	for (owner, id, gross) in owners {
		let fee = sat(gross.to_sat().div_ceil(100).max(1_000));
		total_fees += fee;
		owner.chain().invalidate_caches().await;
		owner.sync_fallback_payouts().await.unwrap();
		let expected_script = &expected[&id.to_string()].1;
		let signed = &parent;
		let index = signed.output.iter().position(|output|
			output.script_pubkey == *expected_script && output.value == gross - fee).unwrap() as u32;
		let point = bitcoin::OutPoint::new(signed.compute_txid(), index);
		assert_eq!(signed.output[index as usize].value, gross - fee);

		let info = ctx.bitcoind().sync_client().custom_get_raw_transaction_info(point.txid, None).unwrap().unwrap();
		assert!(info.confirmations.unwrap_or(0) >= 6);
		let block = info.blockhash.unwrap();
		let header = ctx.bitcoind().sync_client().get_block_header_info(&block).unwrap();
		assert_eq!(ctx.bitcoind().sync_client().get_block_hash(header.height as u64).unwrap(), block);
		assert_eq!(ctx.bitcoind().await_transaction(point.txid).await, *signed);
		let output = ctx.bitcoind().sync_client().get_tx_out(&point.txid, index, Some(true)).unwrap().unwrap();
		assert!(output.confirmations >= 6);
		assert_eq!(output.value, gross - fee);
		assert_eq!(output.script_pub_key.script().unwrap(), *expected_script);
		assert!(paid_points.insert(expected_script.clone(), (point, gross - fee)).is_none());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().state, bark::vtxo::VtxoState::Spent);
		assert!(owner.vtxos().await.unwrap().is_empty());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
		let before_refresh = owner.history().await.unwrap();
		assert!(!before_refresh.iter().any(|movement| movement.subsystem.name == "scuttle.fallback"),
			"Bitcoin settlement must not synthesize a second Ark movement");
		assert_eq!(gross, output.value + fee, "the original Core output and fee still account for all principal");

		assert!(owner.refresh_vtxos([id]).await.unwrap().is_none(), "paid principal must not enter another round");
		owner.sync_fallback_payouts().await.unwrap();
		assert_eq!(owner.history().await.unwrap(), before_refresh, "refresh and receipt replay must not debit or mint again");
	}
	assert_eq!(paid_points.values().map(|entry| entry.1).sum::<bitcoin::Amount>()
		+ total_fees, sat(400_000));

	// Repeat after another real rebuilt tip. Include all generations and all root descendants,
	// not just the selected active receipt, so an extra owner/operator allocation cannot hide.
	let final_height = ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	wm.wait_for_sync_height(final_height).await;
	wm.trigger_sweep().await;
	let final_hash = ctx.bitcoind().sync_client().get_block_hash(final_height.to_u32() as u64).unwrap();
	async {
		loop {
			let rebuilt = db.read(async |tx| {
				let Some(view) = tx.expiry_view().await? else { return Ok(false); };
				Ok(view.processed.height == final_height && view.processed.hash == final_hash
					&& tx.expiry_view_is_current(view).await?)
			}).await.unwrap();
			if rebuilt { break; }
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}.wait_millis(15000).await;
	assert_payouts_once(&ctx, &db, anchor, &expected, &paid_points, &source_path,
		&operator_script, first_height).await;
	while let Ok(claim) = log_claim.try_recv() {
		assert!(!claim.vtxo_ids.iter().any(|id| id.to_point() == anchor || chain.contains(&id.to_point().txid)),
			"the old operator-only claim path must not consume this customer backing");
	}
	for (owner, id, _) in owners {
		owner.chain().invalidate_caches().await;
		owner.sync_fallback_payouts().await.unwrap();
		assert!(owner.vtxos().await.unwrap().is_empty());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().state, bark::vtxo::VtxoState::Spent);
		assert_eq!(owner.history().await.unwrap().iter().filter(|movement|
			movement.subsystem.name == "abandon.fallback_payout").count(), 0);
	}
}

async fn find_movement(wallet: &bark::Wallet, id: MovementId) -> Movement {
	wallet.history().await.expect("history")
		.into_iter().find(|m| m.id == id).expect("movement should exist")
}

/// A finished exit must stay as it is while the wallet stays open.
///
/// The VTXO of a pending exit is spent through arkoor, so the exit ends in
/// `VtxoAlreadySpent` and its movement is `Canceled`. Syncing after the tip
/// moves must not rebuild that state at the new tip, grow the exit history or
/// finish the movement again.
#[tokio::test]
async fn finished_exit_does_not_change_on_sync() {
	let ctx = TestContext::new("bark_sdk/finished_exit_does_not_change_on_sync").await;
	let srv = ctx.captaind("server").create().await;

	let wallet = ctx.bark_sdk("bark", &srv)
		.cfg(|c| c.daemon_manual_sync = true)
		.boarded(sat(400_000))
		.create().await;
	let receiver = ctx.bark_sdk("bark2", &srv).create().await;

	let [boarded] = wallet.vtxos().await.expect("list vtxos")
		.try_into().expect("expected exactly one boarded vtxo");
	let vtxo_id = boarded.vtxo.id();

	// Queue the exit, then spend its VTXO before the exit broadcasts anything.
	wallet.exit_mgr().start_exit_for_entire_wallet().await.expect("starting the exit");
	let address = receiver.new_address().await.expect("new address");
	wallet.send_arkoor_payment(&address, sat(100_000)).await.expect("arkoor send");

	wallet.progress_exits().await.expect("progress exits");
	let exit = wallet.exit_mgr().get_exit_vtxo(vtxo_id).await.expect("exit is tracked");
	assert!(matches!(exit.state(), ExitState::VtxoAlreadySpent(_)), "{:?}", exit.state());
	let movement_id = exit.movement_id().expect("exit has a movement");
	let movement = find_movement(&wallet, movement_id).await;
	assert_eq!(movement.status, MovementStatus::Canceled);

	let mut notifications = wallet.subscribe_notifications();
	ctx.generate_blocks(2).await;
	wallet.sync().await;
	wallet.progress_exits().await.expect("progress exits");

	let after = wallet.exit_mgr().get_exit_vtxo(vtxo_id).await.expect("exit is tracked");
	assert_eq!(after.state(), exit.state());
	assert_eq!(after.history().len(), exit.history().len());
	let after_movement = find_movement(&wallet, movement_id).await;
	assert_eq!(after_movement.status, MovementStatus::Canceled);
	assert_eq!(after_movement.time.completed_at, movement.time.completed_at);
	while let Some(Some(n)) = futures::FutureExt::now_or_never(notifications.next()) {
		if let WalletNotification::MovementUpdated { movement } = n {
			assert_ne!(movement.id, movement_id, "the exit movement must not be updated again");
		}
	}
}

// The two fresh worlds independently exercise stable runtime reconciliation and
// public-open reconciliation: repairing one must not conceal the other's gap.
async fn terminal_exit_history_retries_after_sqlite_fault(reopen: bool) {
	let name = if reopen { "terminal_exit_history_retries_on_public_reopen" }
		else { "terminal_exit_history_retries_on_stable_progress" };
	let ctx = TestContext::new(format!("bark_sdk/{name}")).await;
	let srv = ctx.captaind("server").create().await;
	let mut wallet = ctx.bark_sdk("bark", &srv)
		.cfg(|c| c.daemon_manual_sync = true).boarded(sat(400_000)).create().await;
	let receiver = ctx.bark_sdk("bark2", &srv)
		.cfg(|c| c.daemon_manual_sync = true).create().await;
	wallet.stop_daemon_wait().await.unwrap();
	receiver.stop_daemon_wait().await.unwrap();
	let config = wallet.config().clone();
	let onchain = wallet.onchain().expect("the original board wallet has an onchain wallet");
	let datadir = ctx.datadir.join("bark");
	let mnemonic: bip39::Mnemonic = tokio::fs::read_to_string(datadir.join("mnemonic"))
		.await.unwrap().parse().unwrap();
	let db = SqliteClient::open(datadir.join("db.sqlite")).unwrap();
	let conn = rusqlite::Connection::open(datadir.join("db.sqlite")).unwrap();
	let [boarded] = wallet.vtxos().await.unwrap().try_into().expect("one original board");
	let source_id = boarded.id();
	let original = wallet.get_full_vtxo(source_id).await.unwrap();
	let anchor = original.chain_anchor();
	let funding = ctx.bitcoind().await_transaction(anchor.txid).await;
	let funding_output = funding.output[anchor.vout as usize].clone();
	let core = ctx.bitcoind().sync_client();
	let unspent = core.get_tx_out(&anchor.txid, anchor.vout, Some(true)).unwrap().unwrap();
	assert!(unspent.confirmations > 0);
	assert_eq!(unspent.value, funding_output.value);
	assert_eq!(unspent.script_pub_key.script().unwrap(), funding_output.script_pubkey);

	// The original supported race: queue first, then really spend through Arkoor
	// before exit progression. No terminal state or financial row is injected.
	wallet.exit_mgr().start_exit_for_entire_wallet().await.unwrap();
	let movement_id = wallet.exit_mgr().get_exit_vtxo(source_id).await.unwrap()
		.movement_id().expect("the queued exit has its own movement");
	let pending = db.get_movement_by_id(movement_id).await.unwrap();
	assert_eq!(pending.status, MovementStatus::Pending);
	assert!(pending.time.completed_at.is_none());
	conn.execute_batch(&format!("CREATE TRIGGER issue32_reject_exit_completion \
		BEFORE UPDATE ON bark_movements \
		WHEN OLD.id = {} AND OLD.status = 'pending' AND NEW.status = 'canceled' \
		BEGIN SELECT RAISE(ABORT, 'issue32 native exit completion rejected'); END;", movement_id.0)).unwrap();
	let send_id = wallet.send_arkoor_payment(&receiver.new_address().await.unwrap(), sat(100_000)).await.unwrap();
	receiver.sync().await;
	assert_eq!(receiver.balance().await.unwrap().total(), sat(100_000));
	assert_eq!(wallet.balance().await.unwrap().total(), sat(300_000));
	let mut notifications = wallet.subscribe_notifications();
	wallet.progress_exits().await.unwrap();
	let terminal = db.get_exit_vtxo_entry(&source_id).await.unwrap().unwrap();
	let failed_history = db.get_movement_by_id(movement_id).await.unwrap();
	tokio::fs::write(ctx.datadir.join("terminal-exit-history-cut.json"),
		serde_json::to_vec_pretty(&serde_json::json!({
			"terminal": terminal, "movement_before": pending, "movement_after": failed_history,
		})).unwrap()).await.unwrap();
	assert!(matches!(&terminal.state, ExitState::VtxoAlreadySpent(_)), "{:?}", terminal.state);
	assert_eq!(terminal.movement_id, Some(movement_id));
	assert_eq!(failed_history, pending, "the real terminal state must outlive its failed history write");
	assert_eq!(wallet.get_full_vtxo(source_id).await.unwrap().serialize(), original.serialize());
	wallet.progress_exits().await.unwrap();
	assert_eq!(db.get_exit_vtxo_entry(&source_id).await.unwrap().unwrap(), terminal);
	assert_eq!(db.get_movement_by_id(movement_id).await.unwrap(), pending);
	while let Some(Some(n)) = futures::FutureExt::now_or_never(notifications.next()) {
		if let WalletNotification::MovementUpdated { movement } = n {
			assert_ne!(movement.id, movement_id, "failed completion must not notify success");
		}
	}

	if reopen {
		// Keep the same real fault on first public open: load must report the
		// failure, not silently omit the finished exit and return stale history.
		drop(wallet);
		let outcome = Wallet::open(bitcoin::Network::Regtest,
			WalletSeed::new_from_mnemonic(bitcoin::Network::Regtest, &mnemonic), config.clone(),
			OpenWalletArgs { datadir: Some(datadir.clone()), onchain: Some(onchain.clone()), run_daemon: false, ..Default::default() }).await;
		let error = match outcome { Ok(unexpected) => { drop(unexpected); None }, Err(e) => Some(format!("{e:#}")) };
		tokio::fs::write(ctx.datadir.join("terminal-exit-public-open.json"),
			serde_json::to_vec_pretty(&serde_json::json!({"error": error})).unwrap()).await.unwrap();
		assert!(error.as_deref().is_some_and(|e| e.contains("issue32 native exit completion rejected")),
			"public open must expose the real completion failure: {error:?}");
		assert_eq!(db.get_exit_vtxo_entry(&source_id).await.unwrap().unwrap(), terminal);
		assert_eq!(db.get_movement_by_id(movement_id).await.unwrap(), pending);
		conn.execute_batch("DROP TRIGGER issue32_reject_exit_completion;").unwrap();
		wallet = Wallet::open(bitcoin::Network::Regtest,
			WalletSeed::new_from_mnemonic(bitcoin::Network::Regtest, &mnemonic), config.clone(),
			OpenWalletArgs { datadir: Some(datadir.clone()), onchain: Some(onchain.clone()), run_daemon: false, ..Default::default() }).await.unwrap();
		assert!(wallet.exit_mgr().get_exit_vtxo(source_id).await.is_none(), "finished records stay untracked");
		// Completion happens before open returns, so its notification is not observed here.
		notifications = wallet.subscribe_notifications();
	} else {
		conn.execute_batch("DROP TRIGGER issue32_reject_exit_completion;").unwrap();
		// The state is identical and already terminal. No new state transition
		// may be needed to retry the original movement's failed completion.
		wallet.progress_exits().await.unwrap();
	}
	let finished = db.get_movement_by_id(movement_id).await.unwrap();
	let mut expected = pending.clone();
	expected.status = MovementStatus::Canceled;
	expected.time.completed_at = finished.time.completed_at;
	assert!(finished.time.completed_at.is_some());
	assert_eq!(finished, expected, "only the existing business status and completion time change");
	assert_eq!(find_movement(&wallet, movement_id).await, finished);
	assert_eq!(db.get_exit_vtxo_entry(&source_id).await.unwrap().unwrap(), terminal);
	let mut completions = 0;
	while let Some(Some(n)) = futures::FutureExt::now_or_never(notifications.next()) {
		if let WalletNotification::MovementUpdated { movement } = n {
			if movement.id == movement_id { assert_eq!(movement, finished); completions += 1; }
		}
	}
	assert_eq!(completions, if reopen { 0 } else { 1 });
	let history = wallet.history().await.unwrap();
	let send = history.iter().find(|m| m.id == send_id).unwrap();
	assert_eq!(send.status, MovementStatus::Successful);
	assert_eq!(send.effective_balance, -sat(100_000).to_signed().unwrap());
	assert_eq!(send.offchain_fee, sat(0));
	assert_eq!(send.input_vtxos, vec![source_id]);
	let recipient_history = receiver.history().await.unwrap();
	assert_eq!(recipient_history.len(), 1);
	assert_eq!(recipient_history[0].status, MovementStatus::Successful);
	assert_eq!(recipient_history[0].effective_balance, sat(100_000).to_signed().unwrap());
	assert_eq!(recipient_history[0].offchain_fee, sat(0));
	ctx.generate_blocks(2).await;
	wallet.sync().await;
	wallet.progress_exits().await.unwrap();
	assert_eq!(wallet.history().await.unwrap(), history);
	assert_eq!(db.get_exit_vtxo_entry(&source_id).await.unwrap().unwrap(), terminal);
	while let Some(Some(n)) = futures::FutureExt::now_or_never(notifications.next()) {
		if let WalletNotification::MovementUpdated { movement } = n { assert_ne!(movement.id, movement_id); }
	}
	drop(wallet);
	let wallet = Wallet::open(bitcoin::Network::Regtest,
		WalletSeed::new_from_mnemonic(bitcoin::Network::Regtest, &mnemonic), config,
		OpenWalletArgs { datadir: Some(datadir), onchain: Some(onchain), run_daemon: false, ..Default::default() }).await.unwrap();
	assert_eq!(wallet.history().await.unwrap(), history, "another public open must not finish twice");
	assert_eq!(db.get_exit_vtxo_entry(&source_id).await.unwrap().unwrap(), terminal);
	assert_eq!(wallet.balance().await.unwrap().total(), sat(300_000));
	assert_eq!(receiver.balance().await.unwrap().total(), sat(100_000));
	assert_eq!(receiver.history().await.unwrap(), recipient_history);
	assert_eq!(wallet.get_full_vtxo(source_id).await.unwrap().serialize(), original.serialize());
	let unspent = core.get_tx_out(&anchor.txid, anchor.vout, Some(true)).unwrap().unwrap();
	assert_eq!(unspent.value, funding_output.value);
	assert_eq!(unspent.script_pub_key.script().unwrap(), funding_output.script_pubkey);
	assert_eq!(ctx.bitcoind().await_transaction(anchor.txid).await, funding);
	let mempool = core.get_raw_mempool().unwrap();
	assert!(!original.transactions().any(|step| mempool.contains(&step.tx.compute_txid())),
		"history repair must not publish this canceled exit");
	assert_eq!(history.iter().filter(|m| m.id == movement_id).count(), 1);
	assert_eq!(history.iter().filter(|m| m.status == MovementStatus::Successful)
		.map(|m| m.effective_balance).sum::<bitcoin::SignedAmount>(), sat(300_000).to_signed().unwrap());
}

#[tokio::test]
async fn terminal_exit_history_retries_on_stable_progress() {
	Box::pin(terminal_exit_history_retries_after_sqlite_fault(false)).await;
}

#[tokio::test]
async fn terminal_exit_history_retries_on_public_reopen() {
	Box::pin(terminal_exit_history_retries_after_sqlite_fault(true)).await;
}

/// Dropping one public progress future during a real historical-block read
/// must not remove an initialized exit from this same open manager.
#[tokio::test]
async fn canceled_progress_future_keeps_initialized_exit_tracking() {
	let ctx = TestContext::new("bark_sdk/canceled_progress_future_keeps_initialized_exit_tracking").await;
	let srv = ctx.captaind("server").create().await;
	let target = Arc::new(Mutex::new(None::<u64>));
	let armed = Arc::new(AtomicBool::new(false));
	let hits = Arc::new(AtomicUsize::new(0));
	let entered = Arc::new(tokio::sync::Notify::new());
	let release = Arc::new(tokio::sync::Notify::new());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let proxy_url = format!("http://{}", listener.local_addr().unwrap());
	let upstream = ctx.bitcoind().rpc_url();
	let client = reqwest::Client::builder().no_proxy().build().unwrap();
	let app = Router::new().route("/", post({
		let (target, armed, hits, entered, release) = (target.clone(), armed.clone(), hits.clone(), entered.clone(), release.clone());
		move |headers: HeaderMap, body: Bytes| {
			let (target, armed, hits, entered, release, upstream, client) =
				(target.clone(), armed.clone(), hits.clone(), entered.clone(), release.clone(), upstream.clone(), client.clone());
			async move {
				let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
				if request["method"] == "getblockhash" && request["params"][0].as_u64() == *target.lock().unwrap()
					&& armed.swap(false, Ordering::SeqCst) {
					hits.fetch_add(1, Ordering::SeqCst);
					log::info!("issue31: acknowledged historical claimable-height Core request");
					entered.notify_one();
					release.notified().await;
				}
				let response = client.post(upstream).header(header::AUTHORIZATION, headers[header::AUTHORIZATION].clone())
					.header(header::CONTENT_TYPE, "application/json").body(body).send().await.unwrap();
				(response.status(), response.bytes().await.unwrap())
			}
		}
	})).layer(axum::extract::DefaultBodyLimit::disable());
	let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
	let wallet = ctx.bark_sdk("bark", &srv).cfg(move |cfg| {
		cfg.daemon_manual_sync = true;
		cfg.bitcoind_address = Some(proxy_url);
	}).boarded(sat(400_000)).create().await;
	let [boarded] = wallet.vtxos().await.unwrap().try_into().expect("one genuine board");
	let id = boarded.id();
	let full = wallet.get_full_vtxo(id).await.unwrap();
	wallet.exit_mgr().start_exit_for_entire_wallet().await.unwrap();
	assert_eq!(wallet.exit_mgr().get_exit_vtxo_ids().await, vec![id]);

	let mut awaiting = None;
	for _ in 0..10 {
		wallet.sync_onchain().await.unwrap();
		wallet.progress_exits().await.unwrap();
		let exit = wallet.exit_mgr().get_exit_vtxo(id).await.expect("the original exit stays tracked");
		if let ExitState::AwaitingDelta(state) = exit.state() { awaiting = Some(*state); break; }
		assert!(!matches!(exit.state(), ExitState::Claimable(_) | ExitState::Claimed(_)));
		ctx.generate_blocks(1).await;
		wallet.chain().invalidate_caches().await;
	}
	let awaiting = awaiting.expect("real exit broadcasts and confirms before the cancellation cut");
	let tip = ctx.generate_blocks(blocks_until(&ctx, awaiting.claimable_height) + 1).await;
	assert!(tip > awaiting.claimable_height);
	assert!(tip.to_u32() - awaiting.confirmed_block.height.to_u32() > 6);
	wallet.chain().invalidate_caches().await;
	let before = wallet.exit_mgr().get_exit_vtxo(id).await.unwrap();
	assert!(before.is_initialized());
	assert_eq!(before.state(), &ExitState::AwaitingDelta(awaiting));
	let durable = wallet.exit_mgr().get_exit_status(id, true, false).await.unwrap().unwrap();
	let movement_id = before.movement_id().unwrap();
	let movement = find_movement(&wallet, movement_id).await;
	assert_eq!(movement.status, MovementStatus::Pending);
	let mut notifications = wallet.subscribe_notifications();
	let actual_hash = ctx.bitcoind().sync_client().get_block_hash(awaiting.claimable_height.into()).unwrap();
	*target.lock().unwrap() = Some(awaiting.claimable_height.to_u32().into());
	armed.store(true, Ordering::SeqCst);
	{
		// Both the future and its pinned reference are scoped here. On the
		// acknowledged backend await, drop the actual public future, not
		// just a Pin<&mut _> while retaining the underlying future elsewhere.
		let progress = wallet.progress_exits();
		tokio::pin!(progress);
		tokio::select! {
			_ = entered.notified().wait(Duration::from_secs(30)) => {},
			result = &mut progress => panic!("MISSED_TARGET: progress finished before the historical-block gate: {result:?}"),
		}
	}
	assert_eq!(hits.load(Ordering::SeqCst), 1);
	assert!(!armed.load(Ordering::SeqCst));
	assert_eq!(wallet.exit_mgr().get_exit_status(id, true, false).await.unwrap().unwrap(), durable);
	assert_eq!(wallet.exit_mgr().get_exit_vtxo_ids().await, vec![id]);
	let retained = wallet.exit_mgr().get_exit_vtxo(id).await.expect("same manager must retain the original exit after future cancellation");
	assert_eq!(retained.state(), before.state());
	assert_eq!(retained.history(), before.history());
	assert_eq!(retained.txids(), before.txids());
	assert_eq!(find_movement(&wallet, movement_id).await, movement);
	while let Some(Some(notification)) = futures::FutureExt::now_or_never(notifications.next()) {
		if let WalletNotification::MovementUpdated { movement } = notification {
			assert_ne!(movement.id, movement_id, "canceling only the read future must not finish the exit");
		}
	}

	release.notify_one();
	wallet.progress_exits().await.unwrap();
	let continued = wallet.exit_mgr().get_exit_vtxo(id).await.expect("continue the same tracked exit without reload or a new exit");
	let ExitState::Claimable(claimable) = continued.state() else { panic!("ordinary retry must reach Claimable: {:?}", continued.state()) };
	assert_eq!(claimable.claimable_since.height, awaiting.claimable_height);
	assert_eq!(claimable.claimable_since.hash, actual_hash);
	assert_eq!(continued.id(), id);
	assert_eq!(continued.movement_id(), Some(movement_id));
	assert_eq!(wallet.exit_mgr().get_exit_status(id, true, false).await.unwrap().unwrap().state, *continued.state());
	let coin = ctx.bitcoind().sync_client().get_tx_out(&full.point().txid, full.point().vout, Some(true)).unwrap().unwrap();
	assert!(coin.confirmations >= 6);
	assert_eq!(coin.value, sat(400_000));
	assert_eq!(coin.value, full.amount());
	assert_eq!(coin.script_pub_key.script().unwrap(), full.txout().script_pubkey);
	let history = wallet.history().await.unwrap();
	wallet.progress_exits().await.unwrap();
	assert_eq!(wallet.exit_mgr().get_exit_vtxo(id).await.unwrap().state(), continued.state());
	assert_eq!(wallet.history().await.unwrap(), history);
	assert_eq!(hits.load(Ordering::SeqCst), 1);
	proxy.abort();
}

include!("../scuttle/bark_sdk/exit_fallback.rs");
