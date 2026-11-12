use std::time::Duration;



use bitcoincore_rpc::RpcApi;

use bark::exit::ExitState;
use bitcoin_ext::BlockHeight;
use bitcoin_ext::rpc::BitcoinRpcExt;
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

// Both native cuts retain the same shallow-confirmation and irreversible-exit checks.
async fn assert_swept_exit(
	ctx: &TestContext, wallet: &bark::Wallet, vtxo: ark::VtxoId, parent: &bitcoin::Transaction,
) {
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
	assert_eq!(wallet.get_vtxo_by_id(vtxo).await.unwrap().state, bark::vtxo::VtxoState::Spent);
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

	assert!(matches!(state, Some(ExitState::VtxoAlreadySpent(_))),
		"the committed source must end its ordinary exit as already spent, was {:?}", state,
	);
	// A swept output can never be claimed, so the exit must never have offered it.
	assert!(!claimable, "the exit of a swept vtxo must never become claimable");

}

#[tokio::test]
async fn watchman_sweeps_funding_tx_during_exit() {
	let ctx = TestContext::new("bark_sdk/payouts/watchman_sweeps_funding_tx_during_exit").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
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
	let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
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
			let rows = db.read(async |tx| Ok(tx.query("SELECT DISTINCT a.sweep_tx FROM expiry_allocation_state a \
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

	assert_swept_exit(&ctx, &wallet, vtxo, &parent).await;

	// Observe the real native confirmation projection before consuming its public receipt.
	wm.trigger_sweep().await;
	async {
		loop {
			let paid = db.read(async |tx| Ok(tx.query("SELECT DISTINCT e.vtxo_id FROM expiry_allocation_coin c \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id JOIN vtxo v ON v.vtxo_id=e.vtxo_id \
				JOIN expiry_allocation_state a ON a.id=c.allocation_id \
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
		let fee = sat(gross.to_sat().div_ceil(100).max(2_000));
		total_fees += fee;
		owner.chain().invalidate_caches().await;
		owner.sync_fallback_payouts().await.unwrap();
		let expected_script = &expected[&id.to_string()].1;
		let signed = &parent;
		let index = signed.output.iter().position(|output|
			output.script_pubkey == *expected_script && output.value == gross - fee).unwrap() as u32;
		let point = bitcoin::OutPoint::new(signed.compute_txid(), index);
		assert_eq!(signed.output[index as usize].value, gross - fee);

		let output = ctx.bitcoind().sync_client().get_tx_out(&point.txid, index, Some(true)).unwrap().unwrap();
		assert!(output.confirmations >= 6);
		assert_eq!(output.value, gross - fee);
		assert_eq!(output.script_pub_key.script().unwrap(), *expected_script);
		assert!(paid_points.insert(expected_script.clone(), (point, gross - fee)).is_none());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().state, bark::vtxo::VtxoState::Spent);
		assert!(owner.vtxos().await.unwrap().is_empty());
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
		let payout = owner.fallback_payout(id).await.unwrap().expect("a decided coin reports its payout");
		assert_eq!((payout.status, payout.gross, payout.fee, payout.payout),
			(bark::vtxo::FallbackStatus::Spent, gross, fee, gross - fee));
		assert_eq!(payout.address.script_pubkey(), *expected_script);
		let before_refresh = owner.history().await.unwrap();
		assert!(!before_refresh.iter().any(|movement| movement.subsystem.name == "scuttle.fallback"),
			"Bitcoin settlement must not synthesize a second Ark movement");
		assert_eq!(gross, output.value + fee, "the original Core output and fee still account for all principal");

		let before_refresh_coin = owner.get_vtxo_by_id(id).await.unwrap();
		let refusal = owner.refresh_vtxos([id]).await.expect_err("paid principal must not enter another round");
		assert_eq!(refusal.to_string(), format!("input VTXO {id} is not spendable (state: Spent)"));
		assert_eq!(owner.get_vtxo_by_id(id).await.unwrap(), before_refresh_coin,
			"a refused refresh must leave the retired coin unchanged");
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
				Ok(view.height == final_height && view.hash == final_hash
					&& (tx.expiry_view().await? == Some(view)))
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

include!("exit_fallback.rs");
