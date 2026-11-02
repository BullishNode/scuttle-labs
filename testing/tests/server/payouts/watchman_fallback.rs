use super::*;
use ark::ProtocolEncoding;

#[tokio::test]
pub(super) async fn watchman_sweeps_boards() {
	require_bark_version!(> "0.5.0");

	let ctx = TestContext::new("server/payouts/watchman_sweeps_boards").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());

	let bark1 = ctx.bark("bark1", &srv).funded(sat(200_000)).create().await;

	let board = bark1.board(sat(100_000)).await;
	ctx.generate_blocks(2 * BOARD_CONFIRMATIONS).await;
	let _ = bark1.offchain_balance().await;
	assert_eq!(board.vtxos.len(), 1);
	let client = bark1.client().await;
	let original = client.get_full_vtxo(board.vtxos[0]).await.unwrap();
	assert_eq!(original.amount(), sat(100_000));
	let destination = client.fallback_script();
	let funding = ctx.bitcoind().await_transaction(original.chain_anchor().txid).await;
	original.validate(&funding).unwrap();
	assert_eq!(funding.output[original.chain_anchor().vout as usize].value, sat(100_000));
	drop(client);

	// This path uses a zero-fee parent and a separately funded watchman child.
	let fee_address = wm.wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let operator_script = wm.config().sweep_address.clone().unwrap()
		.require_network(Network::Regtest).unwrap().script_pubkey();

	ctx.refresh_all(&srv, &[&bark1]).await;
	// Complete the refresh while its input can still be forfeited.
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	bark1.sync().await;

	// Stop at the old-board maturity so six confirmation blocks do not
	// also expire the later replacement.
	let height = ctx.bitcoind().get_block_count().await as u32;
	assert!(height < original.expiry_height().to_u32());
	let scan_from = u64::from(original.expiry_height().to_u32());
	let tip = ctx.generate_blocks(original.expiry_height().to_u32() + 1 - height).await;
	let (parent, child, replacement, replacement_funding) =
		tokio::time::timeout(Duration::from_secs(15), async {
		let client = bark1.client().await;
		let replacements = client.vtxos().await.unwrap();
		assert_eq!(replacements.len(), 1);
		let replacement = client.get_full_vtxo(replacements[0].id()).await.unwrap();
		assert_eq!(replacement.amount(), original.amount());
		assert_ne!(replacement.chain_anchor(), original.chain_anchor());
		let replacement_funding = ctx.bitcoind().await_transaction(replacement.chain_anchor().txid).await;
		replacement.validate(&replacement_funding).unwrap();
		drop(client);
		let old_state = db.read(async |tx| tx.get_user_vtxo_by_id(original.id()).await).await.unwrap();
		assert_eq!(old_state.spend_state, server::database::SpendState::Spent);
		let forfeits = db.read(async |tx| Ok(tx.query("SELECT p.id FROM round_part_input i \
			JOIN round_participation p ON p.id=i.participation_id JOIN round r ON r.funding_txid=p.round_id \
			JOIN vtxo v ON v.spent_in_round=r.id WHERE i.vtxo_id=$1 AND v.vtxo_id=$1 \
			AND i.signed_forfeit_tx IS NOT NULL AND p.forfeited_at IS NOT NULL AND r.funding_txid=$2",
			&[&original.id().to_string(), &replacement.chain_anchor().txid.to_string()]).await?)).await.unwrap();
		assert_eq!(forfeits.len(), 1, "operator recovery needs the actual accepted refresh");
		assert!(replacement.expiry_height().to_u32() > tip.to_u32() + 6);
		// Both chain view barriers consume the original financial deadline.
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		let core = ctx.bitcoind().sync_client();
		assert_eq!(core.get_block_count().unwrap(), u64::from(tip.to_u32()));
		let hash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
		// Height alone can precede captaind's background ownership decision.
		loop {
			if db.read(async |tx| {
				let authorized = tx.query_opt("SELECT e.id FROM expiry_entitlement e JOIN LATERAL (SELECT gross_sat,evidence,disposition FROM expiry_allocation_coin WHERE entitlement_id=e.id ORDER BY allocation_id LIMIT 1) first_coin ON TRUE WHERE e.vtxo_id=$1 AND first_coin.gross_sat=100000 \
					AND first_coin.evidence->>'kind'='exchange' \
					AND first_coin.disposition='operator'",
					&[&original.id().to_string()]).await?.is_some();
				if !authorized { return Ok(false); }
				let Some(view) = tx.expiry_view().await? else { return Ok(false); };
				Ok(view.height == tip
					&& view.hash == hash
					&& (tx.expiry_view().await? == Some(view)))
			}).await.unwrap() { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		wm.trigger_sweep().await;
		let packages = wait_watchman_payouts(&ctx, &db, &[original.id().to_string()]).await;
		assert_eq!(packages.len(), 1);
		let (_, (parent, child, _)) = packages.into_iter().next().unwrap();
		(parent, child, replacement, replacement_funding)
	}).await.expect("accepted old board must publish its real funded operator package within15s");
	assert_eq!(parent.input.len(), 1);
	assert_eq!(parent.input[0].previous_output, original.chain_anchor());
	assert_eq!(parent.version.0, 3);
	assert!(parent.vsize() <= 10_000);
	assert_eq!(parent.output.iter().map(|output| output.value).sum::<Amount>(), sat(100_000));
	assert!(parent.output.iter().all(|output| output.is_p2a_fee_anchor() || output.script_pubkey == operator_script));
	assert!(parent.output.iter().all(|output| output.script_pubkey != destination));
	ctx.generate_blocks(6).await;
	assert_canonical_watchman_tx(&ctx, &parent);
	assert_canonical_watchman_tx(&ctx, &child);
	assert_canonical_watchman_tx(&ctx, &replacement_funding);
	// This maturity-window scan also catches a second payment from another
	// funded parent; spending it later cannot conceal its historical output.
	let scan_through = ctx.bitcoind().get_block_count().await;
	assert!(u64::from(replacement.expiry_height().to_u32()) > scan_through);
	let core = ctx.bitcoind().sync_client();
	let mut customer_outputs = Vec::new();
	for height in scan_from..=scan_through {
		let hash = core.get_block_hash(height).unwrap();
		for tx in core.get_block(&hash).unwrap().txdata {
			for (vout, output) in tx.output.iter().enumerate() {
				if output.script_pubkey == destination {
					customer_outputs.push((OutPoint::new(tx.compute_txid(), vout as u32), output.value));
				}
			}
		}
	}
	assert!(customer_outputs.is_empty(), "old forfeited source must not pay its customer in the maturity window: {:?}", customer_outputs);
	let replacement_state = db.read(async |tx| tx.get_user_vtxo_by_id(replacement.id()).await).await.unwrap();
	assert_eq!(replacement_state.spend_state, server::database::SpendState::Spendable);
	assert!(!replacement_state.decided_for_fallback);
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_sweeps_round_vtxos() {
	require_bark_version!(> "0.5.0");

	let ctx = TestContext::new("server/payouts/watchman_sweeps_round_vtxos").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());
	let fee_address = wm.wait_wallet_address().await;
	let mut sponsors = BTreeSet::new();
	// At most three independent backing roots can publish separate packages.
	// Original board confirmation blocks confirm these declared coins too.
	for _ in 0..3 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(100_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32)));
	}
	let bark1 = ctx.bark("bark1", &srv).funded(sat(500_000)).create().await;
	let bark2 = ctx.bark("bark2", &srv).funded(sat(500_000)).create().await;
	bark1.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	bark2.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	let mut originals = Vec::new();
	let mut destinations = Vec::new();
	let mut fundings = BTreeMap::new();
	for bark in [&bark1, &bark2] {
		let client = bark.client().await;
		let coins = client.vtxos().await.unwrap();
		assert_eq!(coins.len(), 1);
		let original = client.get_full_vtxo(coins[0].id()).await.unwrap();
		assert_eq!(original.amount(), sat(200_000));
		let funding = ctx.bitcoind().await_transaction(original.chain_anchor().txid).await;
		original.validate(&funding).unwrap();
		assert_eq!(funding.output[original.chain_anchor().vout as usize].value, sat(200_000));
		fundings.insert(original.chain_anchor(), funding);
		destinations.push(client.fallback_script());
		originals.push(original);
	}
	assert_ne!(destinations[0], destinations[1]);
	ctx.refresh_all(&srv, &[&bark1, &bark2]).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	bark1.sync().await;
	bark2.sync().await;
	let mut replacements = Vec::new();
	for bark in [&bark1, &bark2] {
		let client = bark.client().await;
		let coins = client.vtxos().await.unwrap();
		assert_eq!(coins.len(), 1);
		let replacement = client.get_full_vtxo(coins[0].id()).await.unwrap();
		assert_eq!(replacement.amount(), sat(200_000));
		replacements.push(replacement);
	}
	let round_anchor = replacements[0].chain_anchor();
	assert_eq!(round_anchor, replacements[1].chain_anchor());
	assert!(originals.iter().all(|original| original.chain_anchor() != round_anchor));
	let round_funding = ctx.bitcoind().await_transaction(round_anchor.txid).await;
	assert_eq!(round_funding.output[round_anchor.vout as usize].value, sat(400_000));
	for replacement in &replacements { replacement.validate(&round_funding).unwrap(); }
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	assert_eq!(replacements.iter().map(|coin| coin.amount()).sum::<Amount>(), sat(400_000));
	for coin in &replacements { for step in coin.transactions() {
		assert!(step.tx.output.iter().filter(|output| output.is_p2a_fee_anchor()).all(|output| output.value == Amount::ZERO));
	} }
	fundings.insert(round_anchor, round_funding.clone());
	let mut frontiers = BTreeMap::new();
	for point in originals.iter().map(|coin| coin.chain_anchor()).chain(std::iter::once(round_anchor)) {
		assert!(frontiers.insert(point, fundings[&point].output[point.vout as usize].clone()).is_none());
	}
	assert_eq!(frontiers.len(), 3);
	assert_eq!(frontiers.values().map(|output| output.value).sum::<Amount>(), sat(800_000));
	let core = ctx.bitcoind().sync_client();
	for (point, output) in &frontiers {
		let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 6);
		assert_eq!(actual.value, output.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
	}
	for point in &sponsors {
		assert!(core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap().confirmations >= 2);
	}
	let source_ids = originals.iter().chain(replacements.iter()).map(|coin| coin.id().to_string()).collect::<Vec<_>>();
	assert_eq!(source_ids.iter().collect::<BTreeSet<_>>().len(), 4);
	let operator_script = wm.config().sweep_address.clone().unwrap()
		.require_network(Network::Regtest).unwrap().script_pubkey();
	assert!(destinations.iter().all(|destination| destination != &operator_script));
	let scan_from = u64::from(originals.iter().map(|coin| coin.expiry_height().to_u32()).min().unwrap());
	let tip = ctx.generate_blocks(150).await;
	assert!(originals.iter().chain(replacements.iter()).all(|coin| coin.expiry_height() < tip));
	let packages = tokio::time::timeout(Duration::from_secs(15), async {
		// Attribution, view rebuilding and publication share the original clock.
		for original in &originals {
			let old = db.read(async |tx| tx.get_user_vtxo_by_id(original.id()).await).await.unwrap();
			assert_eq!(old.spend_state, server::database::SpendState::Spent);
			let rows = db.read(async |tx| Ok(tx.query("SELECT p.id FROM round_part_input i \
				JOIN round_participation p ON p.id=i.participation_id JOIN round r ON r.funding_txid=p.round_id \
				JOIN vtxo v ON v.spent_in_round=r.id WHERE i.vtxo_id=$1 AND v.vtxo_id=$1 \
				AND i.signed_forfeit_tx IS NOT NULL AND p.forfeited_at IS NOT NULL AND r.funding_txid=$2",
				&[&original.id().to_string(), &round_anchor.txid.to_string()]).await?)).await.unwrap();
			assert_eq!(rows.len(), 1, "each old board needs its actual accepted forfeit to this round");
		}
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		let hash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
		wait_expiry_allocations(&db, &source_ids, tip, hash).await;
		wm.trigger_sweep().await;
		wait_watchman_payouts(&ctx, &db, &source_ids).await
	}).await.expect("all operator collateral and both customers must publish funded packages within 15s");
	assert!((1..=3).contains(&packages.len()));
	let mut consumed_roots = BTreeSet::new();
	let mut payouts = BTreeMap::new();
	let mut customer_value = Amount::ZERO;
	let mut operator_value = Amount::ZERO;
	for (allocation, (parent, _child, _observed_fee)) in &packages {
		assert_eq!(parent.version.0, 3);
		assert!(parent.vsize() <= 10_000);
		let mut gross = Amount::ZERO;
		for input in &parent.input {
			assert!(consumed_roots.insert(input.previous_output));
			gross += frontiers.get(&input.previous_output).expect("only the three exact roots fund this cut").value;
		}
		assert_eq!(parent.output.iter().map(|output| output.value).sum::<Amount>(), gross);
		let mut anchors = Vec::new();
		for (index, output) in parent.output.iter().enumerate() {
			let point = OutPoint::new(parent.compute_txid(), index as u32);
			if output.is_p2a_fee_anchor() {
				assert_eq!(output.value, Amount::ZERO);
				anchors.push(point);
			} else if let Some(owner) = destinations.iter().position(|destination| destination == &output.script_pubkey) {
				assert_eq!(output.value, sat(198_000));
				assert!(payouts.insert(owner, (point, output.value, *allocation)).is_none());
				customer_value += output.value;
			} else {
				assert_eq!(output.script_pubkey, operator_script);
				operator_value += output.value;
			}
		}
		assert_eq!(anchors.len(), 1);

}
	assert_eq!(consumed_roots, frontiers.keys().copied().collect::<BTreeSet<_>>());
	assert_eq!(payouts.len(), 2);
	assert_eq!(customer_value, sat(396_000));
	assert_eq!(operator_value, sat(404_000));
	ctx.generate_blocks(6).await;
	for (parent, child, _) in packages.values() {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
	}
	let mut observed = BTreeMap::new();
	for height in scan_from..=ctx.bitcoind().get_block_count().await {
		let hash = core.get_block_hash(height).unwrap();
		for tx in core.get_block(&hash).unwrap().txdata {
			for (index, output) in tx.output.iter().enumerate() {
				if destinations.iter().any(|destination| destination == &output.script_pubkey) {
					observed.insert(OutPoint::new(tx.compute_txid(), index as u32), output.value);
				}
			}
		}
	}
	assert_eq!(observed, payouts.values().map(|(point, value, _)| (*point, *value)).collect::<BTreeMap<_, _>>(),
		"both current customers are paid once; old surrendered principal is not paid again");
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_sweeps_arkoor_vtxos_sender_exit() {
	let ctx = TestContext::new("server/payouts/watchman_sweeps_arkoor_vtxos_sender_exit").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());

	// Two ready roots can be committed together or in separate passes. Declare
	// one confirmed sponsor per possible package, retaining any unused coin.
	let fee_address = wm.wait_wallet_address().await;
	let mut sponsors = BTreeSet::new();
	for _ in 0..2 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(100_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32)));
	}

	let bark1 = Arc::new(ctx.bark("bark1", &srv).funded(sat(1_000_000)).create().await);
	let bark2 = Arc::new(ctx.bark("bark2", &srv).funded(sat(1_000_000)).create().await);
	bark1.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	bark2.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	bark1.send_oor(bark2.address().await, sat(50_000)).await;
	bark1.sync().await;
	bark2.sync().await;

	let customer = bark2.client().await;
	let destination = customer.fallback_script();
	let mut customer_sources = Vec::new();
	for coin in customer.vtxos().await.unwrap() {
		customer_sources.push(customer.get_full_vtxo(coin.id()).await.unwrap());
	}
	assert_eq!(customer_sources.len(), 2);
	assert_eq!(customer_sources.iter().map(|coin| coin.amount()).collect::<BTreeSet<_>>(),
		BTreeSet::from([sat(50_000), sat(300_000)]));
	assert_eq!(customer_sources.iter().map(|coin| coin.expiry_height()).collect::<BTreeSet<_>>().len(), 2,
		"the sequential boards retain different original expiry heights");
	let mut customer_fundings = BTreeMap::new();
	for coin in &customer_sources {
		let funding = ctx.bitcoind().await_transaction(coin.chain_anchor().txid).await;
		coin.validate(&funding).unwrap();
		customer_fundings.insert(coin.chain_anchor(), funding);
	}
	let source_ids = customer_sources.iter().map(|coin| coin.id().to_string()).collect::<Vec<_>>();
	let expected_sources = source_ids.iter().cloned().collect::<BTreeSet<_>>();
	assert_eq!(expected_sources.len(), 2);
	drop(customer);
	let claimant = bark1.client().await;
	let claimant_destination = claimant.fallback_script();
	let mut claimant_sources = Vec::new();
	for coin in claimant.vtxos().await.unwrap() {
		let full = claimant.get_full_vtxo(coin.id()).await.unwrap();
		let funding = ctx.bitcoind().await_transaction(full.chain_anchor().txid).await;
		full.validate(&funding).unwrap();
		claimant_sources.push(full);
	}
	assert_eq!(claimant_sources.len(), 2);
	assert!(claimant_sources.iter().all(|coin| coin.amount() == sat(125_000)));
	assert_eq!(claimant_sources.iter().map(|coin| coin.amount()).sum::<Amount>(), sat(250_000));
	let backing_anchors = claimant_sources.iter().map(|coin| coin.chain_anchor().to_string())
		.chain(customer_sources.iter().map(|coin| coin.chain_anchor().to_string())).collect::<BTreeSet<_>>()
		.into_iter().collect::<Vec<_>>();
	let claimant_points = claimant_sources.iter().map(|coin| coin.point()).collect::<BTreeSet<_>>();
	assert_eq!(claimant_points.len(), claimant_sources.len());
	drop(claimant);

	for point in &sponsors {
		assert!(ctx.bitcoind().sync_client().get_tx_out(&point.txid, point.vout, Some(true))
			.unwrap().unwrap().confirmations >= 2);
	}
	let operator_script = wm.config().sweep_address.clone().unwrap()
		.require_network(Network::Regtest).unwrap().script_pubkey();
	assert_ne!(operator_script, destination);
	assert_ne!(claimant_destination, destination);
	let db = Db::connect(&srv.config().postgres).await.unwrap();

	bark1.start_exit_all().await;
	complete_exit(&ctx, &bark1).await;
	for source in &claimant_sources {
		let actual = ctx.bitcoind().await_transaction(source.point().txid).await;
		assert_eq!(actual.output[source.point().vout as usize], source.txout());
	}
	let claim_address = bark1.get_onchain_address().await;
	assert_ne!(claim_address.script_pubkey(), destination);
	assert_ne!(claim_address.script_pubkey(), claimant_destination);
	let scan_from = ctx.bitcoind().get_block_count().await + 1;
	bark1.claim_all_exits(&claim_address).await;
	let core = ctx.bitcoind().sync_client();
	let claims = core.get_raw_mempool().unwrap().into_iter().map(|txid|
		core.get_raw_transaction(&txid, None).unwrap()).filter(|tx|
		tx.input.iter().any(|input| claimant_points.contains(&input.previous_output))).collect::<Vec<_>>();
	assert_eq!(claims.len(), 1);
	let claim = &claims[0];
	assert_eq!(claim.input.len(), claimant_points.len());
	assert_eq!(claim.input.iter().map(|input| input.previous_output).collect::<BTreeSet<_>>(), claimant_points);
	assert_eq!(claim.output.len(), 1);
	assert_eq!(claim.output[0].script_pubkey, claim_address.script_pubkey());
	let claim_fee = sat(250_000).checked_sub(claim.output[0].value).unwrap();
	assert!(claim_fee > Amount::ZERO);

	// Bind each physical checkpoint to its complete signed source path before
	// expiry. The sender's exit exposes the receiver's two backing roots.
	let mut frontiers = BTreeMap::new();
	let mut source_frontiers = BTreeMap::new();
	for coin in &customer_sources {
		let funding = &customer_fundings[&coin.chain_anchor()];
		let mut path = vec![(coin.chain_anchor(), funding.output[coin.chain_anchor().vout as usize].clone())];
		path.extend(coin.transactions().map(|item| {
			let point = OutPoint::new(item.tx.compute_txid(), item.output_idx as u32);
			(point, item.tx.output[item.output_idx].clone())
		}));
		let mut live = Vec::new();
		for (point, expected) in path {
			if let Some(actual) = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap() {
				assert!(actual.confirmations > 0);
				assert_eq!(actual.value, expected.value);
				assert_eq!(actual.script_pub_key.script().unwrap(), expected.script_pubkey);
				live.push((point, expected));
			}
		}
		assert_eq!(live.len(), 1);
		let (point, output) = live.pop().unwrap();
		assert_eq!(output.value, coin.amount());
		assert!(frontiers.insert(point, output).is_none());
		assert!(source_frontiers.insert(coin.id().to_string(), point).is_none());
	}
	assert_eq!(frontiers.len(), 2);
	assert_eq!(frontiers.values().map(|output| output.value).sum::<Amount>(), sat(350_000));

	let before_expiry = ctx.bitcoind().get_block_count().await;
	assert!(customer_sources.iter().all(|coin| u64::from(coin.expiry_height().to_u32()) > before_expiry));
	let early = db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_entitlement e \
		JOIN vtxo v ON v.vtxo_id=e.vtxo_id WHERE v.anchor_point=ANY($1)", &[&backing_anchors])
		.await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(early, 0, "neither an unexpired receiver nor a paid sender may be allocated early");
	let tip = ctx.generate_blocks(150).await;
	assert!(customer_sources.iter().all(|coin| coin.expiry_height() < tip));
	let packages = tokio::time::timeout(Duration::from_secs(15), async {
		// View rebuilding and publication share the original fifteen-second clock.
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		assert_eq!(core.get_block_count().unwrap(), u64::from(tip.to_u32()));
		loop {
			if db.read(async |tx| {
				// The one manual sweep must observe every committed ownership decision.
				let allocated = tx.query("SELECT e.vtxo_id FROM expiry_entitlement e \
					JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
					WHERE e.vtxo_id=ANY($1) AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')", &[&source_ids]).await?
					.into_iter().map(|row| row.get::<_, String>("vtxo_id")).collect::<BTreeSet<_>>();
				Ok(allocated == expected_sources)
			}).await.unwrap() { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		wm.trigger_sweep().await;
		wait_watchman_payouts(&ctx, &db, &source_ids).await
	}).await.expect("all 350k of rightful receiver value must publish funded fallback packages within 15s");
	assert!((1..=2).contains(&packages.len()));
	let mut consumed_roots = BTreeSet::new();
	let mut expected_payouts = BTreeMap::new();
	let mut customer_value = Amount::ZERO;
	let mut operator_value = Amount::ZERO;
	for (_allocation, (parent, _child, _observed_fee)) in &packages {
		assert_eq!(parent.version.0, 3);
		assert!(parent.vsize() <= 10_000);
		let mut gross = Amount::ZERO;
		for input in &parent.input {
			assert!(consumed_roots.insert(input.previous_output));
			gross += frontiers.get(&input.previous_output).expect("no unrelated root may fund receiver fallback").value;
		}
		assert_eq!(parent.output.iter().map(|output| output.value).sum::<Amount>(), gross);
		let payouts = parent.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
		assert_eq!(payouts.len(), 1, "one actual customer output per same-script allocation");
		let expected_fee = customer_sources.iter().filter(|coin| parent.input.iter().any(|input|
			input.previous_output == source_frontiers[&coin.id().to_string()]))
			.map(|coin| sat(coin.amount().to_sat().div_ceil(100).max(1_000))).sum::<Amount>();
		assert_eq!(payouts[0].1.value + expected_fee, gross);
		let payout = OutPoint::new(parent.compute_txid(), payouts[0].0 as u32);
		assert!(expected_payouts.insert(payout, payouts[0].1.value).is_none());
		customer_value += payouts[0].1.value;
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor())
			.map(|(index, output)| (OutPoint::new(parent.compute_txid(), index as u32), output)).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		assert_eq!(anchors[0].1.value, Amount::ZERO);
		let mut remainder = Amount::ZERO;
		for output in &parent.output {
			if output.script_pubkey != destination && !output.is_p2a_fee_anchor() {
				assert_eq!(output.script_pubkey, operator_script);
				remainder += output.value;
			}
		}
		assert_eq!(remainder, expected_fee);
		operator_value += remainder;
}
	assert_eq!(consumed_roots, frontiers.keys().copied().collect::<BTreeSet<_>>());
	assert_eq!(customer_value + operator_value, sat(350_000));
	assert_eq!(operator_value, sat(4_000));
	assert_eq!(customer_value, sat(346_000));

	ctx.generate_blocks(6).await;
	assert_canonical_watchman_tx(&ctx, claim);
	for (parent, child, _) in packages.values() {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
	}
	let actual_claim = core.get_tx_out(&claim.compute_txid(), 0, Some(true)).unwrap().unwrap();
	assert!(actual_claim.confirmations >= 6);
	assert_eq!(actual_claim.value, claim.output[0].value);
	assert_eq!(actual_claim.script_pub_key.script().unwrap(), claim_address.script_pubkey());
	assert_eq!(sat(250_000), actual_claim.value + claim_fee);

	// Include spent historical outputs: a correct UTXO must not hide a second
	// payout, and the already-paid sender has no fallback entitlement here.
	let mut observed_payouts = BTreeMap::new();
	let mut observed_claims = BTreeMap::new();
	for height in scan_from..=ctx.bitcoind().get_block_count().await {
		let hash = core.get_block_hash(height).unwrap();
		for tx in core.get_block(&hash).unwrap().txdata {
			for (index, output) in tx.output.iter().enumerate() {
				let point = OutPoint::new(tx.compute_txid(), index as u32);
				assert_ne!(output.script_pubkey, claimant_destination, "claimed sender must not receive fallback again");
				if output.script_pubkey == destination { observed_payouts.insert(point, output.value); }
				if output.script_pubkey == claim_address.script_pubkey() { observed_claims.insert(point, output.value); }
			}
		}
	}
	assert_eq!(observed_payouts, expected_payouts);
	assert_eq!(observed_claims, BTreeMap::from([(OutPoint::new(claim.compute_txid(), 0), claim.output[0].value)]));
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_sweeps_arkoor_vtxos_receiver_exit() {
	let ctx = TestContext::new("server/payouts/watchman_sweeps_arkoor_vtxos_receiver_exit").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());

	let bark1 = Arc::new(ctx.bark("bark1", &srv).funded(sat(1_000_000)).create().await);
	let bark2 = Arc::new(ctx.bark("bark2", &srv).funded(sat(1_000_000)).create().await);
	bark1.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	bark2.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	bark1.send_oor(bark2.address().await, sat(50_000)).await;
	bark1.sync().await;
	bark2.sync().await;

	let sender = bark1.client().await;
	let destination = sender.fallback_script();
	let mut changes = Vec::new();
	for coin in sender.vtxos().await.unwrap() {
		changes.push(sender.get_full_vtxo(coin.id()).await.unwrap());
	}
	assert_eq!(changes.len(), 2);
	assert!(changes.iter().all(|coin| coin.amount() == sat(125_000)));
	assert_eq!(changes[0].chain_anchor(), changes[1].chain_anchor());
	assert_eq!(changes[0].expiry_height(), changes[1].expiry_height());
	let funding = ctx.bitcoind().await_transaction(changes[0].chain_anchor().txid).await;
	for coin in &changes { coin.validate(&funding).unwrap(); }
	let source_ids = changes.iter().map(|coin| coin.id().to_string()).collect::<Vec<_>>();
	let expected_sources = source_ids.iter().cloned().collect::<BTreeSet<_>>();
	assert_eq!(expected_sources.len(), 2);
	drop(sender);
	let receiver = bark2.client().await;
	let receiver_destination = receiver.fallback_script();
	let mut receiver_sources = Vec::new();
	for coin in receiver.vtxos().await.unwrap() {
		let full = receiver.get_full_vtxo(coin.id()).await.unwrap();
		let funding = ctx.bitcoind().await_transaction(full.chain_anchor().txid).await;
		full.validate(&funding).unwrap();
		receiver_sources.push(full);
	}
	assert_eq!(receiver_sources.iter().map(|coin| coin.amount()).sum::<Amount>(), sat(350_000));
	let backing_anchors = receiver_sources.iter().map(|coin| coin.chain_anchor().to_string())
		.chain(std::iter::once(changes[0].chain_anchor().to_string())).collect::<BTreeSet<_>>()
		.into_iter().collect::<Vec<_>>();
	let receiver_points = receiver_sources.iter().map(|coin| coin.point()).collect::<BTreeSet<_>>();
	assert_eq!(receiver_points.len(), receiver_sources.len());
	drop(receiver);

	// Two ready roots can be committed together or in separate passes. Declare
	// one confirmed sponsor per possible package, retaining any unused coin.
	let fee_address = wm.wait_wallet_address().await;
	let mut sponsors = BTreeSet::new();
	for _ in 0..2 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(100_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32)));
	}
	ctx.generate_blocks(2).await;
	for point in &sponsors {
		assert!(ctx.bitcoind().sync_client().get_tx_out(&point.txid, point.vout, Some(true))
			.unwrap().unwrap().confirmations >= 2);
	}
	let operator_script = wm.config().sweep_address.clone().unwrap()
		.require_network(Network::Regtest).unwrap().script_pubkey();
	assert_ne!(operator_script, destination);
	assert_ne!(receiver_destination, destination);
	let db = Db::connect(&srv.config().postgres).await.unwrap();

	bark2.start_exit_all().await;
	complete_exit(&ctx, &bark2).await;
	for source in &receiver_sources {
		let actual = ctx.bitcoind().await_transaction(source.point().txid).await;
		assert_eq!(actual.output[source.point().vout as usize], source.txout());
	}
	let claim_address = bark2.get_onchain_address().await;
	assert_ne!(claim_address.script_pubkey(), destination);
	assert_ne!(claim_address.script_pubkey(), receiver_destination);
	let scan_from = ctx.bitcoind().get_block_count().await + 1;
	bark2.claim_all_exits(&claim_address).await;
	let core = ctx.bitcoind().sync_client();
	let claims = core.get_raw_mempool().unwrap().into_iter().map(|txid|
		core.get_raw_transaction(&txid, None).unwrap()).filter(|tx|
		tx.input.iter().any(|input| receiver_points.contains(&input.previous_output))).collect::<Vec<_>>();
	assert_eq!(claims.len(), 1);
	let claim = &claims[0];
	assert_eq!(claim.input.len(), receiver_points.len());
	assert_eq!(claim.input.iter().map(|input| input.previous_output).collect::<BTreeSet<_>>(), receiver_points);
	assert_eq!(claim.output.len(), 1);
	assert_eq!(claim.output[0].script_pubkey, claim_address.script_pubkey());
	let claim_fee = sat(350_000).checked_sub(claim.output[0].value).unwrap();
	assert!(claim_fee > Amount::ZERO);

	// Bind each physical checkpoint to its complete signed source path before
	// expiry. The receiver's exit exposes these two sender-owned roots.
	let mut frontiers = BTreeMap::new();
	let mut source_frontiers = BTreeMap::new();
	for coin in &changes {
		let mut path = vec![(coin.chain_anchor(), funding.output[coin.chain_anchor().vout as usize].clone())];
		path.extend(coin.transactions().map(|item| {
			let point = OutPoint::new(item.tx.compute_txid(), item.output_idx as u32);
			(point, item.tx.output[item.output_idx].clone())
		}));
		let mut live = Vec::new();
		for (point, expected) in path {
			if let Some(actual) = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap() {
				assert!(actual.confirmations > 0);
				assert_eq!(actual.value, expected.value);
				assert_eq!(actual.script_pub_key.script().unwrap(), expected.script_pubkey);
				live.push((point, expected));
			}
		}
		assert_eq!(live.len(), 1);
		let (point, output) = live.pop().unwrap();
		assert_eq!(output.value, coin.amount());
		assert!(frontiers.insert(point, output).is_none());
		assert!(source_frontiers.insert(coin.id().to_string(), point).is_none());
	}
	assert_eq!(frontiers.len(), 2);
	assert_eq!(frontiers.values().map(|output| output.value).sum::<Amount>(), sat(250_000));

	let before_expiry = ctx.bitcoind().get_block_count().await;
	assert!(changes.iter().all(|coin| u64::from(coin.expiry_height().to_u32()) > before_expiry));
	let early = db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_entitlement e \
		JOIN vtxo v ON v.vtxo_id=e.vtxo_id WHERE v.anchor_point=ANY($1)", &[&backing_anchors])
		.await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(early, 0, "neither an unexpired sender nor a paid receiver may be allocated early");
	let tip = ctx.generate_blocks(150).await;
	assert!(changes.iter().all(|coin| coin.expiry_height() < tip));
	let packages = tokio::time::timeout(Duration::from_secs(5), async {
		// These chain view barriers consume the original five-second clock.
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		assert_eq!(core.get_block_count().unwrap(), u64::from(tip.to_u32()));
		loop {
			if db.read(async |tx| {
				// The one manual sweep must observe every committed ownership decision.
				let allocated = tx.query("SELECT e.vtxo_id FROM expiry_entitlement e \
					JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
					WHERE e.vtxo_id=ANY($1) AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')", &[&source_ids]).await?
					.into_iter().map(|row| row.get::<_, String>("vtxo_id")).collect::<BTreeSet<_>>();
				Ok(allocated == expected_sources)
			}).await.unwrap() { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		wm.trigger_sweep().await;
		wait_watchman_payouts(&ctx, &db, &source_ids).await
	}).await.expect("all250k of rightful sender change must publish funded fallback packages within5s");
	assert!((1..=2).contains(&packages.len()));
	let mut consumed_roots = BTreeSet::new();
	let mut expected_payouts = BTreeMap::new();
	let mut customer_value = Amount::ZERO;
	let mut operator_value = Amount::ZERO;
	for (_allocation, (parent, _child, _observed_fee)) in &packages {
		assert_eq!(parent.version.0, 3);
		assert!(parent.vsize() <= 10_000);
		let mut gross = Amount::ZERO;
		for input in &parent.input {
			assert!(consumed_roots.insert(input.previous_output));
			gross += frontiers.get(&input.previous_output).expect("no unrelated root may fund sender fallback").value;
		}
		assert_eq!(parent.output.iter().map(|output| output.value).sum::<Amount>(), gross);
		let payouts = parent.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
		assert_eq!(payouts.len(), 1, "one actual customer output per same-script allocation");
		let expected_fee = sat(gross.to_sat().div_ceil(100));
		assert_eq!(payouts[0].1.value + expected_fee, gross);
		let payout = OutPoint::new(parent.compute_txid(), payouts[0].0 as u32);
		assert!(expected_payouts.insert(payout, payouts[0].1.value).is_none());
		customer_value += payouts[0].1.value;
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor())
			.map(|(index, output)| (OutPoint::new(parent.compute_txid(), index as u32), output)).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		assert_eq!(anchors[0].1.value, Amount::ZERO);
		let mut remainder = Amount::ZERO;
		for output in &parent.output {
			if output.script_pubkey != destination && !output.is_p2a_fee_anchor() {
				assert_eq!(output.script_pubkey, operator_script);
				remainder += output.value;
			}
		}
		assert_eq!(remainder, expected_fee);
		operator_value += remainder;
}
	assert_eq!(consumed_roots, frontiers.keys().copied().collect::<BTreeSet<_>>());
	assert_eq!(customer_value + operator_value, sat(250_000));
	assert_eq!(operator_value, sat(2_500));
	assert_eq!(customer_value, sat(247_500));

	ctx.generate_blocks(6).await;
	assert_canonical_watchman_tx(&ctx, claim);
	for (parent, child, _) in packages.values() {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
	}
	let actual_claim = core.get_tx_out(&claim.compute_txid(), 0, Some(true)).unwrap().unwrap();
	assert!(actual_claim.confirmations >= 6);
	assert_eq!(actual_claim.value, claim.output[0].value);
	assert_eq!(actual_claim.script_pub_key.script().unwrap(), claim_address.script_pubkey());
	assert_eq!(sat(350_000), actual_claim.value + claim_fee);

	// Include spent historical outputs: a correct UTXO must not hide a second
	// payout, and the already-paid receiver has no fallback entitlement here.
	let mut observed_payouts = BTreeMap::new();
	let mut observed_claims = BTreeMap::new();
	for height in scan_from..=ctx.bitcoind().get_block_count().await {
		let hash = core.get_block_hash(height).unwrap();
		for tx in core.get_block(&hash).unwrap().txdata {
			for (index, output) in tx.output.iter().enumerate() {
				let point = OutPoint::new(tx.compute_txid(), index as u32);
				assert_ne!(output.script_pubkey, receiver_destination, "claimed receiver must not receive fallback again");
				if output.script_pubkey == destination { observed_payouts.insert(point, output.value); }
				if output.script_pubkey == claim_address.script_pubkey() { observed_claims.insert(point, output.value); }
			}
		}
	}
	assert_eq!(observed_payouts, expected_payouts);
	assert_eq!(observed_claims, BTreeMap::from([(OutPoint::new(claim.compute_txid(), 0), claim.output[0].value)]));
	failures.assert_empty();
}

// Shared read-only observers; callers retain their original enclosing deadline.
async fn wait_expiry_allocations(
	db: &Db, source_ids: &[String], tip: bitcoin_ext::BlockHeight, hash: bitcoin::BlockHash,
) {
	let expected = source_ids.iter().cloned().collect::<BTreeSet<_>>();
	loop {
		if db.read(async |tx| {
			let allocated = tx.query("SELECT e.vtxo_id FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
				WHERE e.vtxo_id=ANY($1) AND EXISTS(SELECT 1 FROM expiry_allocation_state a WHERE a.id=c.allocation_id AND a.status<>'conflicted')",
				&[&source_ids]).await?.into_iter().map(|row| row.get::<_, String>(0)).collect::<BTreeSet<_>>();
			let Some(view) = tx.expiry_view().await? else { return Ok(false); };
			Ok(allocated == expected && view.height == tip && view.hash == hash
				&& tx.expiry_view().await? == Some(view))
		}).await.unwrap() { return; }
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
}

async fn wait_watchman_payouts(
	ctx: &TestContext, db: &Db, source_ids: &[String],
) -> BTreeMap<i64, (Transaction, Transaction, i64)> {
	let core = ctx.bitcoind().sync_client();
	let expected = source_ids.iter().cloned().collect::<BTreeSet<_>>();
	loop {
		let rows = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id,a.id,a.sweep_tx FROM expiry_entitlement e \
			JOIN expiry_allocation_coin c ON c.entitlement_id=e.id JOIN expiry_allocation_state a ON a.id=c.allocation_id \
			WHERE e.vtxo_id=ANY($1) AND a.status<>'conflicted' AND a.broadcast_at IS NOT NULL", &[&source_ids]).await?)).await.unwrap();
		let mut seen = BTreeSet::new();
		let mut packages = BTreeMap::new();
		for row in rows {
			let parent = deserialize::<Transaction>(row.get("sweep_tx")).unwrap();
			let Some((child, fee)) = observed_payout_fee_child(ctx, &parent) else { continue; };
			assert!(seen.insert(row.get::<_, String>("vtxo_id")));
			let package = (parent, child, fee);
			if let Some(previous) = packages.insert(row.get::<_, i64>("id"), package.clone()) { assert_eq!(previous, package); }
		}
		if seen == expected && packages.values().all(|(parent, child, _)|
			core.get_raw_transaction(&parent.compute_txid(), None).ok().as_ref() == Some(parent)
			&& core.get_raw_transaction(&child.compute_txid(), None).ok().as_ref() == Some(child)) { return packages; }
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
}

fn watchman_fee(ctx: &TestContext, tx: &Transaction) -> Amount {
	let core = ctx.bitcoind().sync_client();
	let inputs = tx.input.iter().map(|input| {
		let previous = core.get_raw_transaction(&input.previous_output.txid, None).unwrap();
		previous.output[input.previous_output.vout as usize].value
	}).sum::<Amount>();
	inputs.checked_sub(tx.output.iter().map(|output| output.value).sum::<Amount>()).unwrap()
}

#[tokio::test]
pub(super) async fn watchman_sweeps_lightning_vtxos() {
	require_bark_version!(> "0.5.0");

	let ctx = TestContext::new("server/payouts/watchman_sweeps_lightning_vtxos").await;
	let ln = ctx.new_lightning_setup("ln").await;
	// Observe and mine packages on the node used by both daemons.
	// Zero-fee parent relay to another node has its own asynchronous delay.
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).lightningd(&ln.internal).funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![VtxoTarget { count: 10, amount: sat(10_000) }];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());

	// Three physical roots can become ready together or in separate passes.
	let fee_address = wm.wait_wallet_address().await;
	let mut sponsors = BTreeSet::new();
	for _ in 0..3 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(100_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32)));
	}
	let bark1 = Arc::new(ctx.bark("bark1", &srv).funded(sat(1_000_000)).create().await);
	let bark2 = Arc::new(ctx.bark("bark2", &srv).funded(sat(1_000_000)).create().await);
	bark1.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	bark2.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	let mut boards = Vec::new();
	let mut destinations = Vec::new();
	let mut fundings = BTreeMap::new();
	for bark in [&bark1, &bark2] {
		let client = bark.client().await;
		let coins = client.vtxos().await.unwrap();
		assert_eq!(coins.len(), 1);
		let board = client.get_full_vtxo(coins[0].id()).await.unwrap();
		assert_eq!(board.amount(), sat(300_000));
		let funding = ctx.bitcoind().await_transaction(board.chain_anchor().txid).await;
		board.validate(&funding).unwrap();
		fundings.insert(board.chain_anchor(), funding);
		boards.push(board);
		destinations.push(client.fallback_script());
	}
	assert_ne!(destinations[0], destinations[1]);
	let invoice = bark2.bolt11_invoice(sat(50_000)).await.invoice;
	let parsed: bark::lightning_invoice::Bolt11Invoice = invoice.parse().unwrap();
	let hash = PaymentHash::from(&parsed);
	srv.wait_for_vtxopool(&ctx).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let initial_pool = futures::TryStreamExt::try_collect::<Vec<_>>(db.load_vtxopool().await.unwrap()).await.unwrap();
	assert_eq!(initial_pool.len(), 10);
	assert!(initial_pool.iter().all(|coin| coin.amount() == sat(10_000)));
	let pool_anchor = initial_pool[0].chain_anchor();
	assert!(initial_pool.iter().all(|coin| coin.chain_anchor() == pool_anchor));
	assert!(boards.iter().all(|board| board.chain_anchor() != pool_anchor));
	let pool_funding = ctx.bitcoind().await_transaction(pool_anchor.txid).await;
	assert_eq!(pool_funding.output[pool_anchor.vout as usize].value, sat(100_000));
	for coin in &initial_pool { coin.validate(&pool_funding).unwrap(); }
	fundings.insert(pool_anchor, pool_funding);
	let payer = bark1.clone();
	let receiver = bark2.clone();
	tokio::join!(
		async move { payer.pay_lightning_wait(&invoice, None).await; },
		async move { receiver.lightning_receive_all().wait_millis(60_000).await; },
	);

	// Read the registered outputs, without replacing the real receive action.
	let mut customers = BTreeMap::new();
	for (owner, bark) in [&bark1, &bark2].into_iter().enumerate() {
		let client = bark.client().await;
		let mut total = Amount::ZERO;
		for coin in client.vtxos().await.unwrap() {
			let full = client.get_full_vtxo(coin.id()).await.unwrap();
			assert!(matches!(full.policy(), ark::VtxoPolicy::Pubkey(_)));
			full.validate(&fundings[&full.chain_anchor()]).unwrap();
			total += full.amount();
			assert!(customers.insert(full.id().to_string(), (full, owner)).is_none());
		}
		assert_eq!(total, sat(if owner == 0 { 250_000 } else { 350_000 }));
	}
	assert!(customers.contains_key(&boards[1].id().to_string()));
	let converted = customers.iter().filter(|(_, (coin, owner))| *owner == 1 && coin.id() != boards[1].id())
		.map(|(id, _)| id.clone()).collect::<BTreeSet<_>>();
	assert!(!converted.is_empty());
	assert_eq!(converted.iter().map(|id| customers[id].0.amount()).sum::<Amount>(), sat(50_000));
	let mut customer_gross_by_expiry = BTreeMap::<(usize, u32), u64>::new();
	for (source, owner) in customers.values() {
		*customer_gross_by_expiry.entry((*owner, source.expiry_height().to_u32())).or_default() += source.amount().to_sat();
	}
	let expected_fees_by_expiry = customer_gross_by_expiry.iter().map(|(group, gross)|
		(*group, gross.div_ceil(100).max(1_000))).collect::<BTreeMap<_, _>>();
	let pool_remainder = futures::TryStreamExt::try_collect::<Vec<_>>(db.load_vtxopool().await.unwrap()).await.unwrap();
	assert!(pool_remainder.iter().all(|coin| coin.chain_anchor() == pool_anchor));
	assert_eq!(pool_remainder.iter().map(|coin| coin.amount()).sum::<Amount>(), sat(50_000));
	let mut frontiers = BTreeMap::new();
	for point in boards.iter().map(|coin| coin.chain_anchor()).chain(std::iter::once(pool_anchor)) {
		assert!(frontiers.insert(point, fundings[&point].output[point.vout as usize].clone()).is_none());
	}
	assert_eq!(frontiers.len(), 3);
	assert_eq!(frontiers.values().map(|output| output.value).sum::<Amount>(), sat(700_000));
	let core = ctx.bitcoind().sync_client();
	for (point, output) in &frontiers {
		let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations > 0);
		assert_eq!(actual.value, output.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
	}
	for point in &sponsors {
		let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 2);
		assert_eq!(actual.value, sat(100_000));
		assert_eq!(actual.script_pub_key.script().unwrap(), fee_address.script_pubkey());
	}
	let operator_script = wm.config().sweep_address.clone().unwrap()
		.require_network(Network::Regtest).unwrap().script_pubkey();
	assert!(destinations.iter().all(|destination| destination != &operator_script));
	let scan_from = u64::from(boards.iter().map(|coin| coin.expiry_height().to_u32())
		.chain(std::iter::once(initial_pool[0].expiry_height().to_u32())).min().unwrap());
	let tip = ctx.generate_blocks(150).await;
	assert!(customers.values().all(|(coin, _)| coin.expiry_height() < tip));
	let packages = tokio::time::timeout(Duration::from_secs(5), async {
		// Source attribution, rebuilt views and publication share the original five seconds.
		let funding_rows = db.read(async |tx| Ok(tx.query("SELECT a.id,a.lightning_htlc_subscription_id AS subscription_id FROM lightning_payment_attempt a \
			JOIN lightning_htlc_subscription s ON s.id=a.lightning_htlc_subscription_id AND s.payment_hash=a.payment_hash \
			WHERE a.payment_hash=$1", &[&hash.to_string()]).await?)).await.unwrap();
		assert_eq!(funding_rows.len(), 1, "this invoice must have one actual internal funding attempt");
		let attempt: i64 = funding_rows[0].get("id");
		let subscription: i64 = funding_rows[0].get("subscription_id");
		let preimage = db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().unwrap();
		assert_eq!(preimage.compute_payment_hash(), hash);
		let incoming = db.read(async |tx| Ok(tx.query("SELECT l.vtxo_id,h.direction::TEXT,h.payment_hash,h.offchain_resolution::TEXT,h.chain_resolution::TEXT \
			FROM lightning_payment_attempt_htlc_vtxo l LEFT JOIN vtxo v ON v.vtxo_id=l.vtxo_id \
			LEFT JOIN htlc_vtxo h ON h.id=v.id WHERE l.lightning_payment_attempt_id=$1", &[&attempt]).await?)).await.unwrap();
		assert!(!incoming.is_empty());
		let mut operators = BTreeMap::new();
		let mut incoming_ids = BTreeSet::new();
		for row in incoming {
			assert_eq!(row.get::<_, String>("direction"), "incoming");
			assert_eq!(row.get::<_, String>("payment_hash"), hash.to_string());
			assert_ne!(row.get::<_, Option<String>>("offchain_resolution").as_deref(), Some("revoked"));
			assert_ne!(row.get::<_, Option<String>>("chain_resolution").as_deref(), Some("revoked"));
			let id = row.get::<_, String>("vtxo_id");
			assert!(incoming_ids.insert(id.clone()));
			let state = db.read(async |tx| tx.get_user_vtxo_by_id(ark::VtxoId::from_str(&id).unwrap()).await).await.unwrap();
			assert_eq!(state.spend_state, server::database::SpendState::LnSpent);
			let coin = state.vtxo;
			assert_eq!(coin.chain_anchor(), boards[0].chain_anchor());
			assert_eq!(coin.policy().as_server_htlc_send().unwrap().payment_hash, hash);
			coin.validate(&fundings[&coin.chain_anchor()]).unwrap();
			assert!(operators.insert(id, coin).is_none());
		}
		assert_eq!(operators.values().map(|coin| coin.amount()).sum::<Amount>(), sat(50_000));
		let grants = db.read(async |tx| Ok(tx.query("SELECT v.vtxo_id,h.direction::TEXT,h.payment_hash,h.offchain_resolution::TEXT \
			FROM vtxo v LEFT JOIN htlc_vtxo h ON h.id=v.id JOIN lightning_htlc_subscription s ON s.id=v.lightning_htlc_subscription_id \
			WHERE s.id=$1 AND s.status='settled'", &[&subscription]).await?)).await.unwrap();
		assert!(!grants.is_empty());
		let mut grant_ids = BTreeSet::new();
		let mut claimed_outputs = BTreeSet::new();
		let mut grant_total = Amount::ZERO;
		for row in grants {
			assert_eq!(row.get::<_, String>("direction"), "outgoing");
			assert_eq!(row.get::<_, String>("payment_hash"), hash.to_string());
			assert_eq!(row.get::<_, Option<String>>("offchain_resolution").as_deref(), Some("fulfilled"));
			let id = row.get::<_, String>("vtxo_id");
			assert!(grant_ids.insert(id.clone()));
			let grant = db.read(async |tx| tx.get_user_vtxo_by_id(id.parse().unwrap()).await).await.unwrap().vtxo;
			assert_eq!(grant.chain_anchor(), pool_anchor);
			assert_eq!(grant.policy().as_server_htlc_recv().unwrap().payment_hash, hash);
			grant.validate(&fundings[&pool_anchor]).unwrap();
			grant_total += grant.amount();
			let outputs = customers.iter().filter(|(_, (coin, owner))| *owner == 1
				&& coin.transactions().any(|step| step.tx.input.iter().any(|input| input.previous_output == grant.point())))
				.collect::<Vec<_>>();
			assert_eq!(outputs.len(), 1, "each fulfilled grant must have one authenticated signed recipient output");
			let (output_id, (actual, _)) = outputs[0];
			assert!(claimed_outputs.insert(output_id.clone()));
			assert_eq!(actual.amount(), grant.amount());
			let registered = db.read(async |tx| tx.get_user_vtxo_by_id(actual.id()).await).await.unwrap();
			assert_ne!(registered.spend_state, server::database::SpendState::Unregistered);
			assert!(registered.oor_spent_txid.is_none(), "the selected converted coin must be the live terminal");
			assert_eq!(registered.vtxo.to_bare().serialize(), actual.to_bare().serialize());
			registered.vtxo.validate(&fundings[&pool_anchor]).unwrap();
		}
		assert_eq!(grant_total, sat(50_000));
		assert_eq!(claimed_outputs, converted);
		for coin in &pool_remainder {
			let state = db.read(async |tx| tx.get_user_vtxo_by_id(coin.id()).await).await.unwrap();
			assert!(state.oor_spent_txid.is_none());
			assert_eq!(state.vtxo.to_bare().serialize(), coin.to_bare().serialize());
			state.vtxo.validate(&fundings[&pool_anchor]).unwrap();
			assert!(operators.insert(coin.id().to_string(), coin.inner().clone()).is_none());
		}
		assert_eq!(operators.values().map(|coin| coin.amount()).sum::<Amount>(), sat(100_000));
		assert!(customers.keys().all(|id| !operators.contains_key(id) && !grant_ids.contains(id)));
		let source_ids = customers.keys().chain(operators.keys()).cloned().collect::<Vec<_>>();
		let expected = source_ids.iter().cloned().collect::<BTreeSet<_>>();
		assert_eq!(source_ids.len(), expected.len());
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		let blockhash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
		wait_expiry_allocations(&db, &source_ids, tip, blockhash).await;
		// The sweep rebuilds the view itself. Do not wait for the periodic rebuild before the trigger.
		wm.trigger_sweep().await;
		let packages = wait_watchman_payouts(&ctx, &db, &source_ids).await;
		packages
	}).await.expect("all actual Lightning source rights must publish funded packages within the original 5s");
	assert!((1..=3).contains(&packages.len()));
	let mut consumed_roots = BTreeSet::new();
	let mut payouts = BTreeMap::new();
	let mut customer_value = Amount::ZERO;
	let mut operator_value = Amount::ZERO;
	for (allocation, (parent, _child, _observed_fee)) in &packages {
		assert_eq!(parent.version.0, 3);
		assert!(parent.vsize() <= 10_000);
		let mut gross = Amount::ZERO;
		for input in &parent.input {
			assert!(consumed_roots.insert(input.previous_output));
			gross += frontiers.get(&input.previous_output).expect("only the three original roots fund this cut").value;
		}
		assert_eq!(parent.output.iter().map(|output| output.value).sum::<Amount>(), gross);
		let mut anchors = Vec::new();
		let mut owners = BTreeSet::new();
		for (index, output) in parent.output.iter().enumerate() {
			let point = OutPoint::new(parent.compute_txid(), index as u32);
			if output.is_p2a_fee_anchor() {
				assert_eq!(output.value, Amount::ZERO);
				anchors.push(point);
			} else if let Some(owner) = destinations.iter().position(|destination| destination == &output.script_pubkey) {
				assert!(output.value > Amount::ZERO);
				assert!(owners.insert(owner), "one actual output per customer script in each parent");
				assert!(payouts.insert(point, (output.value, owner, *allocation)).is_none());
				customer_value += output.value;
			} else {
				assert_eq!(output.script_pubkey, operator_script);
				operator_value += output.value;
			}
		}
		assert_eq!(anchors.len(), 1);
	}
	assert_eq!(consumed_roots, frontiers.keys().copied().collect::<BTreeSet<_>>());
	assert!(!payouts.is_empty());
	for owner in 0..destinations.len() {
		let gross = customer_gross_by_expiry.iter().filter(|((index, _), _)| *index == owner)
			.map(|(_, gross)| *gross).sum::<u64>();
		let fees = expected_fees_by_expiry.iter().filter(|((index, _), _)| *index == owner)
			.map(|(_, fee)| *fee).sum::<u64>();
		assert_eq!(payouts.values().filter(|(_, index, _)| *index == owner)
			.map(|(amount, _, _)| *amount).sum::<Amount>(), sat(gross - fees));
	}
	let output_fees = sat(expected_fees_by_expiry.values().copied().sum::<u64>());
	assert_eq!(customer_value, sat(600_000) - output_fees);
	assert_eq!(operator_value, sat(100_000) + output_fees);
	ctx.generate_blocks(6).await;
	for (parent, child, _) in packages.values() {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
	}
	let mut observed = BTreeMap::new();
	for height in scan_from..=ctx.bitcoind().get_block_count().await {
		for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
			for (index, output) in tx.output.iter().enumerate() {
				if destinations.iter().any(|destination| destination == &output.script_pubkey) {
					observed.insert(OutPoint::new(tx.compute_txid(), index as u32), output.value);
				}
			}
		}
	}
	assert_eq!(observed, payouts.iter().map(|(point, (value, _, _))| (*point, *value)).collect::<BTreeMap<_, _>>(),
		"pay actual current customer rights once, never refund the fulfilled payer source or its grant ancestor");
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_sweeps_round_leftovers_after_exits() {
	require_bark_version!(> "0.5.0");
	let ctx = TestContext::new("server/payouts/watchman_sweeps_round_leftovers_after_exits").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
		cfg.watchman.claim_chunksize = 20.try_into().unwrap();
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());
	// Any partial-ready grouping has at most18 disjoint roots. Keep the whole declared fee budget explicit.
	let fee_address = wm.wait_wallet_address().await;
	assert!(fee_address.script_pubkey().is_p2tr());
	let mut sponsors = BTreeMap::new();
	for i in 0..18 {
		let amount = sat(if i == 17 { 11_113 } else { 11_111 });
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), amount).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == amount).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32), outputs[0].1.clone()).is_none());
	}
	assert_eq!(sponsors.values().map(|output| output.value).sum::<Amount>(), sat(200_000));
	let mut barks = Vec::new();
	let mut board_funding_txids = Vec::new();
	let mut board_ids = Vec::new();
	for i in 0..10 {
		let bark = ctx.bark(&format!("bark{}", i), &srv).funded(sat(500_000)).create().await;
		let board = bark.board(sat(200_000)).await;
		assert_eq!(board.vtxos.len(), 1);
		board_ids.push(board.vtxos[0]);
		board_funding_txids.push(board.funding_tx.txid);
		barks.push(bark);
	}
	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let mut originals = Vec::new();
	let mut destinations = Vec::new();
	for (index, bark) in barks.iter().enumerate() {
		bark.sync().await;
		let wallet = bark.client().await;
		let source = wallet.get_full_vtxo(board_ids[index]).await.unwrap();
		assert_eq!(source.amount(), sat(200_000));
		assert_eq!(source.chain_anchor().txid, board_funding_txids[index]);
		let funding = ctx.bitcoind().await_transaction(source.chain_anchor().txid).await;
		source.validate(&funding).unwrap();
		originals.push(source);
		destinations.push(wallet.fallback_script());
	}
	assert_eq!(destinations.iter().map(|destination| destination.clone()).collect::<BTreeSet<_>>().len(), 10);
	let bark_refs = barks.iter().collect::<Vec<_>>();
	ctx.refresh_all(&srv, &bark_refs).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	let mut replacements = Vec::new();
	for bark in &barks {
		bark.sync().await;
		let ids = bark.vtxo_ids().await;
		assert_eq!(ids.len(), 1);
		let source = bark.client().await.get_full_vtxo(ids[0]).await.unwrap();
		assert_eq!(source.amount(), sat(200_000));
		replacements.push(source);
	}
	let round_anchor = replacements[0].chain_anchor();
	assert!(replacements.iter().all(|coin| coin.chain_anchor() == round_anchor));
	let round_funding = ctx.bitcoind().await_transaction(round_anchor.txid).await;
	for coin in &replacements { coin.validate(&round_funding).unwrap(); }

	let core = ctx.bitcoind().sync_client();
	let scan_from = ctx.bitcoind().get_block_count().await + 1;
	let claim_address = barks[0].get_onchain_address().await;
	barks[0].start_exit_all().await;
	barks[1].start_exit_all().await;
	tokio::join!(
		async {
			complete_exit(&ctx, &barks[0]).await;
			barks[0].claim_all_exits(&claim_address).await;
		},
		async { complete_exit(&ctx, &barks[1]).await; },
	);
	let backing_anchors = originals.iter().chain(replacements.iter()).map(|coin| coin.chain_anchor().to_string())
		.collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
	let operator_script = wm.config().sweep_address.clone().unwrap().require_network(Network::Regtest).unwrap().script_pubkey();
	assert!(destinations.iter().all(|destination| destination != &operator_script));
	let tip = ctx.generate_blocks(150).await;
	let (packages, roots, expected) = tokio::time::timeout(Duration::from_secs(15), async {
		// Attribution, completed view and all funded publication share the original clock.
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		assert_eq!(core.get_block_count().unwrap(), u64::from(tip.to_u32()));
		let hash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
		assert!(core.get_tx_out(&replacements[0].point().txid, replacements[0].point().vout, Some(true)).unwrap().is_none());
		let unclaimed = core.get_tx_out(&replacements[1].point().txid, replacements[1].point().vout, Some(true)).unwrap().unwrap();
		assert!(unclaimed.confirmations >= 6);
		assert_eq!(unclaimed.value, sat(200_000));
		assert_eq!(unclaimed.script_pub_key.script().unwrap(), replacements[1].output_script_pubkey());
		let rows = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM vtxo WHERE anchor_point=ANY($1) \
			AND frontier_at IS NOT NULL AND confirmed_height<=$2 AND onchain_spent_height IS NULL AND expiry<=$2",
			&[&backing_anchors, &(tip.to_u32() as i32)]).await?)).await.unwrap();
		let mut roots = Vec::new();
		for row in rows {
			let id = ark::VtxoId::from_str(row.get("vtxo_id")).unwrap();
			if id == replacements[1].id() { continue; } // This user-only200k remains outside the sweep.
			let root = db.read(async |tx| tx.get_bare_vtxo_by_id(id).await).await.unwrap().vtxo;
			if let Some(actual) = core.get_tx_out(&root.point().txid, root.point().vout, Some(true)).unwrap() {
				assert!(actual.confirmations >= 1);
				assert_eq!(actual.value, root.amount());
				assert_eq!(actual.script_pub_key.script().unwrap(), root.output_script_pubkey());
				assert!(matches!(root.policy(), ark::ServerVtxoPolicy::Expiry(_) | ark::ServerVtxoPolicy::HarkLeaf(_)));
				roots.push(root);
			}
		}
		assert!(!roots.is_empty() && roots.len() <= 18);
		let semantic = originals.iter().cloned().map(|coin| (ark::ServerVtxo::from(coin), None))
			.chain(replacements.iter().enumerate().skip(2).map(|(owner, coin)| (ark::ServerVtxo::from(coin.clone()), Some(owner))));
		let mut expected = BTreeMap::new();
		for (coin, owner) in semantic {
			assert!(coin.expiry_height() <= tip);
			let root_id = roots.iter().find(|root| root.point() == coin.chain_anchor() || coin.transactions().any(|step| bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32) == root.point())).map(|root| root.id()).expect("a rightful terminal needs exactly one current root");
			let point = roots.iter().find(|root| root.id() == root_id).unwrap().point();
			assert!(expected.insert(coin.id().to_string(), (coin.clone(), owner, point)).is_none());
		}
		assert_eq!(expected.len(), 18);
		assert_eq!(roots.iter().map(|root| root.amount()).sum::<Amount>(), sat(3_600_000));
		for txid in &board_funding_txids { assert!(roots.iter().any(|root| root.point().txid == *txid)); }
		loop {
			let owned = watchman_confirmed_outpoints(wm).await;
			if sponsors.keys().all(|point| owned.contains(point)) { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		for (point, output) in &sponsors {
			let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations >= 6);
			assert_eq!(actual.value, output.value);
			assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
		}
		let source_ids = expected.keys().cloned().collect::<Vec<_>>();
		wait_expiry_allocations(&db, &source_ids, tip, hash).await;
		wm.trigger_sweep().await;
		let packages = wait_watchman_payouts(&ctx, &db, &source_ids).await;
		assert!(!packages.is_empty() && packages.len() <= roots.len());
		(packages, roots, expected)
	}).await.expect("all actual funded leftover payouts must appear within the original fifteen seconds");
	let tip = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(tip).await;
	wm.wait_for_sync_height(tip).await;
	let mut consumed_roots = BTreeSet::new();
	let mut terminal_outputs = BTreeMap::new();
	let mut operator = Amount::ZERO;
	let mut terminal_count = 0usize;
	for (parent, child, observed_fee) in packages.values() {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
		assert_eq!(core.custom_get_raw_transaction_info(parent.compute_txid(), None).unwrap().unwrap().blockhash,
			core.custom_get_raw_transaction_info(child.compute_txid(), None).unwrap().unwrap().blockhash);
		assert_eq!(parent.version.0, 3);
		assert!(parent.vsize() <= 10_000);
		assert_eq!(watchman_fee(&ctx, parent), Amount::ZERO);
		let mut wanted = BTreeMap::new();
		let mut operator_gross = Amount::ZERO;
		let mut group_terminals = 0usize;
		for input in &parent.input {
			assert!(roots.iter().any(|root| root.point() == input.previous_output));
			assert!(consumed_roots.insert(input.previous_output));
			for (coin, owner, point) in expected.values() {
				if *point != input.previous_output { continue; }
				group_terminals += 1;
				if let Some(owner) = owner {
					assert!(wanted.insert(destinations[*owner].clone(), coin.amount() - sat(2_000)).is_none());
					operator_gross += sat(2_000);
				} else { operator_gross += coin.amount(); }
			}
		}
		assert!(group_terminals > 0 && parent.input.len() <= group_terminals);
		assert!(wanted.len() <= group_terminals);
		assert!(parent.vsize() <= 67 + 148 * group_terminals);
		terminal_count += group_terminals;
		wanted.insert(operator_script.clone(), operator_gross);
		assert_eq!(parent.output.iter().filter(|output| !output.is_p2a_fee_anchor()).map(|output|
			(output.script_pubkey.clone(), output.value)).collect::<BTreeMap<_, _>>(), wanted);
		assert_eq!(wanted.len() + 1, parent.output.len());
		operator += operator_gross;
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor()).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		let fee = watchman_fee(&ctx, child);
		assert!(fee > Amount::ZERO);
		assert!(fee <= bitcoin::FeeRate::from_sat_per_vb(25).unwrap() * (parent.weight() + child.weight()),
			"this fixed fixture budget only qualifies the bounded fee profile; no funding rescue");
		assert_eq!(fee.to_sat(), u64::try_from(*observed_fee).unwrap());
		for (index, output) in parent.output.iter().enumerate() {
			let point = OutPoint::new(parent.compute_txid(), index as u32);
			if output.is_p2a_fee_anchor() { assert!(core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().is_none()); }
			else { terminal_outputs.insert(point, output.clone()); }
		}
	}
	assert_eq!(terminal_count, 18);
	assert_eq!(consumed_roots, roots.iter().map(|root| root.point()).collect());
	assert_eq!(operator, sat(10 * 200_000 + 8 * 2_000));
	let mut claims = BTreeMap::new();
	let mut observed = BTreeMap::new();
	let exit_txs = replacements.iter().take(2).flat_map(|coin| coin.transactions()).map(|step| step.tx).collect::<Vec<_>>();
	let exit_anchors = exit_txs.iter().flat_map(|tx| tx.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor())
		.map(|(index, _)| OutPoint::new(tx.compute_txid(), index as u32))).collect::<BTreeSet<_>>();
	for height in scan_from..=ctx.bitcoind().get_block_count().await {
		for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
			if tx.input.iter().any(|input| input.previous_output == replacements[0].point()) { claims.insert(tx.compute_txid(), tx.clone()); }
			let native_fee_child = tx.input.iter().any(|input| exit_anchors.contains(&input.previous_output));
			for (index, output) in tx.output.iter().enumerate().filter(|(_, output)|
				destinations.iter().any(|destination| destination == &output.script_pubkey) || output.script_pubkey == claim_address.script_pubkey()) {
				// Upstream CPFP can return wallet change to this unused claim
				// address. It spends a known exit anchor, not another Ark claim.
				if native_fee_child && output.script_pubkey == claim_address.script_pubkey() { continue; }
				observed.insert(OutPoint::new(tx.compute_txid(), index as u32), output.clone());
			}
		}
	}
	assert_eq!(claims.len(), 1);
	let claim = claims.values().next().unwrap();
	assert_eq!(claim.input.len(), 1);
	assert_eq!(claim.input[0].previous_output, replacements[0].point());
	assert_eq!(claim.output.len(), 1);
	assert_eq!(claim.output[0].script_pubkey, claim_address.script_pubkey());
	let claim_fee = watchman_fee(&ctx, claim);
	assert!(claim_fee > Amount::ZERO);
	assert_eq!(claim.output[0].value + claim_fee, sat(200_000));
	assert_canonical_watchman_tx(&ctx, claim);
	let mut wanted = terminal_outputs.iter().filter(|(_, output)| destinations.iter().any(|destination| destination == &output.script_pubkey))
		.map(|(point, output)| (*point, output.clone())).collect::<BTreeMap<_, _>>();
	assert_eq!(wanted.len(), 8);
	assert_eq!(wanted.values().map(|output| output.value).sum::<Amount>(), sat(1_584_000));
	assert!(wanted.insert(OutPoint::new(claim.compute_txid(), 0), claim.output[0].clone()).is_none());
	assert_eq!(observed, wanted, "eight rightful fallbacks and one real native claim, no duplicate or user1 refund");
	let unclaimed = core.get_tx_out(&replacements[1].point().txid, replacements[1].point().vout, Some(true)).unwrap().unwrap();
	assert!(unclaimed.confirmations >= 6);
	assert_eq!(unclaimed.value, sat(200_000));
	assert_eq!(unclaimed.script_pub_key.script().unwrap(), replacements[1].output_script_pubkey());
	assert_canonical_watchman_tx(&ctx, &core.get_raw_transaction(&replacements[1].point().txid, None).unwrap());
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_sweeps_vtxopool_with_exit() {
	require_bark_version!(> "0.5.0");
	let ctx = TestContext::new("server/payouts/watchman_sweeps_vtxopool_with_exit").await;
	let ln = ctx.new_lightning_setup("ln").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).funded(btc(10)).lightningd(&ln.internal).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![
			VtxoTarget { count: 9, amount: sat(1_000) },
			VtxoTarget { count: 9, amount: sat(10_000) },
			VtxoTarget { count: 3, amount: btc(1) },
		];
		cfg.vtxopool.vtxo_lifetime = BlockDelta::new(144);
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());
	let core = ctx.bitcoind().sync_client();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let operator_script = wm.config().sweep_address.clone().unwrap().require_network(Network::Regtest).unwrap().script_pubkey();

	// Fee children use separate, declared watchman capital. Never reduce customer principal.
	let fee_address = wm.wait_wallet_address().await;
	let mut sponsors = BTreeMap::new();
	for _ in 0..4 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(25_000)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(25_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32), outputs[0].1.clone()).is_none());
	}
	let bark1 = Arc::new(ctx.bark("bark1", &srv).funded(sat(1_000_000)).create().await);
	let bark2 = Arc::new(ctx.bark("bark2", &srv).funded(sat(1_000_000)).create().await);
	bark1.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	bark2.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	let mut boards = Vec::new();
	let mut fundings = BTreeMap::new();
	for bark in [&bark1, &bark2] {
		let client = bark.client().await;
		let coins = client.vtxos().await.unwrap();
		assert_eq!(coins.len(), 1);
		let board = client.get_full_vtxo(coins[0].id()).await.unwrap();
		assert_eq!(board.amount(), sat(300_000));
		let funding = core.get_raw_transaction(&board.chain_anchor().txid, None).unwrap();
		board.validate(&funding).unwrap();
		assert!(fundings.insert(board.chain_anchor(), funding).is_none());
		boards.push(board);
	}
	let invoice = bark2.bolt11_invoice(sat(50_000)).await.invoice;
	let parsed: bark::lightning_invoice::Bolt11Invoice = invoice.parse().unwrap();
	let hash = PaymentHash::from(&parsed);
	srv.wait_for_vtxopool(&ctx).await;
	let initial_pool = futures::TryStreamExt::try_collect::<Vec<_>>(db.load_vtxopool().await.unwrap()).await.unwrap();
	assert_eq!(initial_pool.len(), 21);
	assert_eq!(initial_pool.iter().filter(|coin| coin.amount() == sat(1_000)).count(), 9);
	assert_eq!(initial_pool.iter().filter(|coin| coin.amount() == sat(10_000)).count(), 9);
	assert_eq!(initial_pool.iter().filter(|coin| coin.amount() == btc(1)).count(), 3);
	let pool_anchor = initial_pool[0].chain_anchor();
	assert!(initial_pool.iter().all(|coin| coin.chain_anchor() == pool_anchor));
	let pool_funding = core.get_raw_transaction(&pool_anchor.txid, None).unwrap();
	for coin in &initial_pool { coin.validate(&pool_funding).unwrap(); }
	assert_eq!(pool_funding.output[pool_anchor.vout as usize].value, sat(300_099_000));
	assert!(fundings.insert(pool_anchor, pool_funding).is_none());
	let payer = bark1.clone();
	let receiver = bark2.clone();
	tokio::join!(
		async move { payer.pay_lightning_wait(&invoice, None).await; },
		async move { receiver.lightning_receive_all().wait_millis(60_000).await; },
	);
	bark1.sync().await;
	bark2.sync().await;
	let mut customers = BTreeMap::new();
	let mut destinations = Vec::new();
	for (owner, bark) in [&bark1, &bark2].into_iter().enumerate() {
		let client = bark.client().await;
		destinations.push(client.fallback_script());
		let mut total = Amount::ZERO;
		for coin in client.vtxos().await.unwrap() {
			let full = client.get_full_vtxo(coin.id()).await.unwrap();
			assert!(matches!(full.policy(), ark::VtxoPolicy::Pubkey(_)));
			full.validate(&fundings[&full.chain_anchor()]).unwrap();
			total += full.amount();
			assert!(customers.insert(full.id().to_string(), (full, owner)).is_none());
		}
		assert_eq!(total, sat(if owner == 0 { 250_000 } else { 350_000 }));
	}
	assert_ne!(destinations[0], destinations[1]);
	assert!(destinations.iter().all(|destination| destination != &operator_script));
	let incoming = db.read(async |tx| Ok(tx.query("SELECT l.vtxo_id,h.direction::TEXT,h.offchain_resolution::TEXT \
		FROM lightning_payment_attempt a \
		JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id \
		JOIN vtxo v ON v.vtxo_id=l.vtxo_id JOIN htlc_vtxo h ON h.id=v.id \
		WHERE a.payment_hash=$1 AND a.lightning_htlc_subscription_id IS NOT NULL AND h.payment_hash=a.payment_hash",
		&[&hash.to_string()]).await?)).await.unwrap();
	assert!(!incoming.is_empty());
	let mut operators = BTreeMap::new();
	let mut incoming_ids = BTreeSet::new();
	for row in incoming {
		assert_eq!(row.get::<_, String>("direction"), "incoming");
		assert_ne!(row.get::<_, Option<String>>("offchain_resolution").as_deref(), Some("revoked"));
		let id = row.get::<_, String>("vtxo_id");
		let coin = db.read(async |tx| tx.get_user_vtxo_by_id(ark::VtxoId::from_str(&id).unwrap()).await).await.unwrap().vtxo;
		assert_eq!(coin.chain_anchor(), boards[0].chain_anchor());
		assert_eq!(coin.policy().as_server_htlc_send().unwrap().payment_hash, hash);
		coin.validate(&fundings[&coin.chain_anchor()]).unwrap();
		assert!(incoming_ids.insert(id.clone()));
		assert!(operators.insert(id, coin).is_none());
	}
	assert_eq!(operators.values().map(|coin| coin.amount()).sum::<Amount>(), sat(50_000));
	// Native Pool state also retains allocation change omitted from the live depth-limited pool.
	let remainder = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM vtxo WHERE anchor_point=$1 AND spend_state='pool'",
		&[&pool_anchor.to_string()]).await?)).await.unwrap();
	for row in remainder {
		let state = db.read(async |tx| tx.get_user_vtxo_by_id(ark::VtxoId::from_str(row.get("vtxo_id")).unwrap()).await).await.unwrap();
		assert_eq!(state.spend_state, server::database::SpendState::Pool);
		assert!(state.oor_spent_txid.is_none() && state.spent_in_round.is_none() && state.offboarded_in.is_none());
		let coin = state.vtxo;
		assert_eq!(coin.chain_anchor(), pool_anchor);
		coin.validate(&fundings[&pool_anchor]).unwrap();
		assert!(operators.insert(coin.id().to_string(), coin).is_none());
	}
	assert_eq!(operators.values().map(|coin| coin.amount()).sum::<Amount>(), sat(300_099_000));
	assert!(customers.keys().all(|id| !operators.contains_key(id)));
	bark1.start_exit_all().await;
	bark2.start_exit_all().await;
	let claim_destination = bark1.get_onchain_address().await;
	tokio::join!(
		async {
			complete_exit(&ctx, &bark1).await;
			bark1.claim_all_exits(&claim_destination).await;
		},
		async { complete_exit(&ctx, &bark2).await; },
	);
	let operator_ids = operators.keys().cloned().collect::<Vec<_>>();
	let before_expiry = ctx.bitcoind().get_block_count().await as u32;
	assert!(operators.values().all(|coin| coin.expiry_height().to_u32() > before_expiry));
	let early = db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) AS n FROM expiry_entitlement e WHERE e.vtxo_id=ANY($1)",
		&[&operator_ids]).await?.get::<_, i64>("n"))).await.unwrap();
	assert_eq!(early, 0, "unexpired pool and discharged source must not receive an expiry allocation");
	let tip = ctx.generate_blocks(150).await;
	let backing_anchors = fundings.keys().map(ToString::to_string).collect::<Vec<_>>();
	let (packages, roots) = tokio::time::timeout(Duration::from_secs(10), async {
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		let rows = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM vtxo WHERE anchor_point=ANY($1) \
			AND frontier_at IS NOT NULL AND confirmed_height<=$2 AND onchain_spent_height IS NULL AND expiry<=$2",
			&[&backing_anchors, &(tip.to_u32() as i32)]).await?)).await.unwrap();
		let mut roots = Vec::new();
		for row in rows {
			let root = db.read(async |tx| tx.get_bare_vtxo_by_id(ark::VtxoId::from_str(row.get("vtxo_id")).unwrap()).await).await.unwrap().vtxo;
			let selected = server::database::expiry_inventory::BackingSnapshot { roots: vec![root.clone()], regular: Vec::new() };
			if !operators.values().any(|coin| selected.roots.iter().find(|root| root.point() == coin.chain_anchor() || coin.transactions().any(|step| bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32) == root.point())).map(|root| root.id()).is_some()) { continue; }
			let actual = core.get_tx_out(&root.point().txid, root.point().vout, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations > 0);
			assert_eq!(actual.value, root.amount());
			assert_eq!(actual.script_pub_key.script().unwrap(), root.output_script_pubkey());
			roots.push(root);
		}
		assert!(!roots.is_empty());
		assert_eq!(roots.iter().map(|root| root.amount()).sum::<Amount>(), sat(300_099_000));
		let hash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
		wait_expiry_allocations(&db, &operator_ids, tip, hash).await;
		loop {
			if sponsors.keys().all(|point| core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().is_some_and(|output| output.confirmations >= 6)) {
				let owned = watchman_confirmed_outpoints(wm).await;
				if sponsors.keys().all(|point| owned.contains(point)) { break; }
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		wm.trigger_sweep().await;
		let packages = wait_watchman_payouts(&ctx, &db, &operator_ids).await;
		(packages, roots)
	}).await.expect("all actual operator rights must publish funded packages within the original ten seconds");
	assert!((1..=3).contains(&packages.len()));
	let mut spent_roots = BTreeSet::new();
	let mut spent_sponsors = BTreeSet::new();
	let mut retained = BTreeSet::new();
	let mut operator_value = Amount::ZERO;
	for (parent, child, observed_fee) in packages.values() {
		assert_eq!(parent.version.0, 3);
		assert_eq!(watchman_fee(&ctx, parent), Amount::ZERO);
		for input in &parent.input {
			assert!(roots.iter().any(|root| root.point() == input.previous_output));
			assert!(spent_roots.insert(input.previous_output));
		}
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor()).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		assert_eq!(anchors[0].1.value, Amount::ZERO);
		for output in parent.output.iter().filter(|output| !output.is_p2a_fee_anchor()) {
			assert_eq!(output.script_pubkey, operator_script);
			operator_value += output.value;
		}
		assert_eq!(child.input.iter().filter(|input| input.previous_output == OutPoint::new(parent.compute_txid(), anchors[0].0 as u32)).count(), 1);
		for input in child.input.iter().filter(|input| input.previous_output != OutPoint::new(parent.compute_txid(), anchors[0].0 as u32)) {
			assert!(sponsors.contains_key(&input.previous_output));
			assert!(spent_sponsors.insert(input.previous_output));
		}
		assert!(!child.output.is_empty() && child.output.iter().all(|output| output.value > Amount::ZERO));
		for (index, _) in child.output.iter().enumerate() { retained.insert(OutPoint::new(child.compute_txid(), index as u32)); }
		let fee = watchman_fee(&ctx, child);
		assert!(fee > Amount::ZERO);
		assert_eq!(fee.to_sat(), u64::try_from(*observed_fee).unwrap());
	}
	assert_eq!(spent_roots, roots.iter().map(|root| root.point()).collect());
	assert_eq!(operator_value, sat(300_099_000));
	retained.extend(sponsors.keys().filter(|point| !spent_sponsors.contains(point)).copied());
	let final_tip = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(final_tip).await;
	wm.wait_for_sync_height(final_tip).await;
	for (parent, child, _) in packages.values() {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
	}
	tokio::time::timeout(Duration::from_secs(10), async {
		loop {
			let confirmed = db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) AS n FROM expiry_entitlement e \
				JOIN expiry_allocation_coin c ON c.entitlement_id=e.id JOIN expiry_allocation_state a ON a.id=c.allocation_id \
				WHERE e.vtxo_id=ANY($1) AND a.status<>'conflicted' AND a.status='confirmed' AND a.confirmed_hash IS NOT NULL",
				&[&operator_ids]).await?.get::<_, i64>("n"))).await.unwrap();
			if confirmed == operator_ids.len() as i64 && retained.is_subset(&watchman_confirmed_outpoints(wm).await) { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("all child change and unused fee capital must remain watchman property");
	bark1.sync().await;
	bark2.sync().await;
	let receiver_points = customers.values().filter(|(_, owner)| *owner == 1).map(|(coin, _)| (coin.point(), coin.txout())).collect::<BTreeMap<_, _>>();
	assert_eq!(receiver_points.values().map(|output| output.value).sum::<Amount>(), sat(350_000));
	for (point, output) in &receiver_points {
		let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 6);
		assert_eq!(actual.value, output.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
	}
	let first = boards.iter().map(|coin| {
		let info = core.custom_get_raw_transaction_info(coin.chain_anchor().txid, None).unwrap().unwrap();
		core.get_block_header_info(&info.blockhash.unwrap()).unwrap().height
	}).min().unwrap();
	let mut claims = BTreeMap::new();
	let sender_points = customers.values().filter(|(_, owner)| *owner == 0).map(|(coin, _)| coin.point()).collect::<BTreeSet<_>>();
	for height in first as u64..=u64::from(final_tip.to_u32()) {
		for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
			assert!(tx.output.iter().all(|output| destinations.iter().all(|destination| destination != &output.script_pubkey)),
				"neither customer exit may also receive a canonical fallback payment");
			if tx.input.iter().any(|input| sender_points.contains(&input.previous_output)) {
				assert!(claims.insert(tx.compute_txid(), tx).is_none());
			}
		}
	}
	assert_eq!(claims.len(), 1);
	let claim = claims.values().next().unwrap();
	assert_canonical_watchman_tx(&ctx, claim);
	assert_eq!(claim.input.iter().map(|input| input.previous_output).collect::<BTreeSet<_>>(), sender_points);
	assert!(claim.output.iter().all(|output| output.script_pubkey == claim_destination.script_pubkey()));
	assert!(claim.output.iter().map(|output| output.value).sum::<Amount>() > Amount::ZERO);
	assert_eq!(claim.output.iter().map(|output| output.value).sum::<Amount>() + watchman_fee(&ctx, claim), sat(250_000));
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_sweeps_offboard_connectors() {
	let ctx = TestContext::new("server/payouts/watchman_sweeps_offboard_connectors").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
		cfg.watchman.claim_chunksize = 20.try_into().unwrap();
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());
	let fee_address = wm.wait_wallet_address().await;
	let mut available = BTreeMap::new();
	for _ in 0..4 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(100_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(available.insert(OutPoint::new(txid, outputs[0].0 as u32), outputs[0].1.clone()).is_none());
	}
	let bark = ctx.bark_sdk("bark1", &srv).funded(sat(1_000_000))
		.boarded(sat(200_000)).boarded(sat(200_000)).boarded(sat(200_000)).boarded(sat(200_000)).create().await;
	bark.sync().await;
	let vtxos = bark.vtxos().await.unwrap();
	assert_eq!(vtxos.len(), 4, "should hold four board vtxos");
	let max_expiry = vtxos.iter().map(|v| v.vtxo.expiry_height()).max().unwrap();
	let destination = bark.fallback_script();
	let mut originals = Vec::new();
	let mut board_roots = BTreeMap::new();
	for coin in &vtxos {
		let source = bark.get_full_vtxo(coin.id()).await.unwrap();
		let funding = ctx.bitcoind().await_transaction(source.chain_anchor().txid).await;
		source.validate(&funding).unwrap();
		assert_eq!(source.amount(), sat(200_000));
		assert!(board_roots.insert(source.chain_anchor(), funding.output[source.chain_anchor().vout as usize].clone()).is_none());
		originals.push(source);
	}
	assert_eq!(board_roots.values().map(|output| output.value).sum::<Amount>(), sat(800_000));
	let core = ctx.bitcoind().sync_client();
	let scan_from = ctx.bitcoind().get_block_count().await + 1;
	let payout = ctx.bitcoind().get_new_address();
	assert_ne!(payout.script_pubkey(), destination);
	let offboard_txid = bark.offboard_all(payout.clone()).await.unwrap().txid;
	let tip = ctx.generate_blocks(1).await;
	let offboard = core.get_raw_transaction(&offboard_txid, None).unwrap();
	assert_eq!(offboard.output[0].script_pubkey, payout.script_pubkey());
	assert!(offboard.output[0].value > Amount::ZERO);
	let connector_point = OutPoint::new(offboard_txid, 1);
	let connector_id = ark::VtxoId::from(connector_point);
	assert_eq!(offboard.output[1].value, sat(1_320));
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let operator_script = wm.config().sweep_address.clone().unwrap().require_network(Network::Regtest).unwrap().script_pubkey();
	assert_ne!(operator_script, destination);
	let connector_expiry = max_expiry.to_u32() + 144;
	let early_height = max_expiry.to_u32() + 72;
	ctx.generate_blocks(early_height - tip.to_u32()).await;
	let mut operator_total = Amount::ZERO;
	let mut miner_total = Amount::ZERO;
	let mut all_packages = BTreeMap::new();
	let board_ids = originals.iter().map(|coin| coin.id().to_string()).collect::<Vec<_>>();
	let mut all_sources = board_ids.iter().cloned().collect::<BTreeSet<_>>();
	assert_eq!(all_sources.len(), 4);
	assert!(all_sources.insert(connector_id.to_string()));
	for late in [false, true] {
		let height = ctx.bitcoind().get_block_count().await as u32;
		if late {
			assert!(height < connector_expiry);
			ctx.generate_blocks(connector_expiry - height).await;
		} else { assert_eq!(height, early_height); }
		let tip = bitcoin_ext::BlockHeight::new(if late { connector_expiry } else { early_height });
		let source_ids = if late { vec![connector_id.to_string()] } else { board_ids.clone() };
		let roots = if late { BTreeMap::from([(connector_point, offboard.output[1].clone())]) } else { board_roots.clone() };
		let packages = tokio::time::timeout(Duration::from_secs(15), async {
			// All new attribution, completed-view checks and publication share this stage's original clock.
			let stored = db.read(async |tx| Ok(tx.query_one("SELECT signed_tx,user_fee_sat FROM offboards WHERE txid=$1",
				&[&offboard_txid.to_string()]).await?)).await.unwrap();
			assert_eq!(deserialize::<Transaction>(stored.get("signed_tx")).unwrap(), offboard);
			assert_eq!(offboard.output[0].value + sat(u64::try_from(stored.get::<_, i64>("user_fee_sat")).unwrap()), sat(800_000));
			for original in &originals {
				let state = db.read(async |tx| tx.get_user_vtxo_by_id(original.id()).await).await.unwrap();
				assert_eq!(state.offboarded_in, Some(offboard_txid));
				assert_eq!(state.spend_state, server::database::SpendState::Spent);
				assert_eq!(state.vtxo.to_bare().serialize(), original.to_bare().serialize());
			}
			let connector = db.read(async |tx| tx.get_server_vtxo_by_id(connector_id).await).await.unwrap();
			assert_eq!(connector.vtxo.point(), connector_point);
			assert_eq!(connector.vtxo.chain_anchor(), connector_point);
			assert_eq!(connector.vtxo.amount(), sat(1_320));
			assert_eq!(connector.vtxo.expiry_height().to_u32(), connector_expiry);
			assert_eq!(connector.vtxo.exit_delta(), BlockDelta::ZERO);
			assert_eq!(*connector.vtxo.policy(), ark::ServerVtxoPolicy::ServerOwned);
			assert!(originals.iter().all(|original| original.server_pubkey() == connector.vtxo.server_pubkey()));
			connector.vtxo.validate(&offboard).unwrap();
			if !late {
				assert_eq!(connector.spend_state, server::database::SpendState::OffboardConnector);
				assert!(!connector.decided_for_fallback);
				assert!(core.get_tx_out(&connector_point.txid, connector_point.vout, Some(true)).unwrap().is_some());
			}
			srv.wait_for_sync_height(tip).await;
			wm.wait_for_sync_height(tip).await;
			assert_eq!(core.get_block_count().unwrap(), u64::from(tip.to_u32()));
			let hash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
			// Wait for the same original source rights, without mirroring their diagnostic evidence JSON.
			loop {
				let seen = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM expiry_entitlement WHERE vtxo_id=ANY($1)",
					&[&source_ids]).await?.into_iter().map(|row| row.get::<_, String>(0)).collect::<BTreeSet<_>>())).await.unwrap();
				if seen == source_ids.iter().cloned().collect() { break; }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
			loop {
				if db.read(async |tx| {
					let Some(view) = tx.expiry_view().await? else { return Ok(false); };
					Ok(view.height == tip && view.hash == hash && tx.expiry_view().await? == Some(view))
				}).await.unwrap() { break; }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
			let owned = watchman_confirmed_outpoints(wm).await;
			assert!(available.keys().all(|point| owned.contains(point)));
			for (point, expected) in &available {
				let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
				assert!(actual.confirmations >= 6);
				assert_eq!(actual.value, expected.value);
				assert_eq!(actual.script_pub_key.script().unwrap(), expected.script_pubkey);
			}
			wm.trigger_sweep().await;
			wait_watchman_payouts(&ctx, &db, &source_ids).await
		}).await.expect("all current-view funded operator packages must publish within this original 15s");
		assert!((1..=(if late { 1 } else { 4 })).contains(&packages.len()));
		let mut spent_roots = BTreeSet::new();
		let mut spent_fees = BTreeSet::new();
		let mut new_change = BTreeMap::new();
		for (allocation, (parent, child, observed_fee)) in &packages {
			assert_eq!(parent.version.0, 3);
			assert!(parent.vsize() <= 10_000);
			for input in &parent.input {
				assert!(roots.contains_key(&input.previous_output));
				assert!(spent_roots.insert(input.previous_output));
			}
			let gross = parent.input.iter().map(|input| roots[&input.previous_output].value).sum::<Amount>();
			assert_eq!(watchman_fee(&ctx, parent), Amount::ZERO);
			let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor()).collect::<Vec<_>>();
			assert_eq!(anchors.len(), 1);
			assert_eq!(anchors[0].1.value, Amount::ZERO);
			let anchor = OutPoint::new(parent.compute_txid(), anchors[0].0 as u32);
			let outputs = parent.output.iter().filter(|output| !output.is_p2a_fee_anchor()).collect::<Vec<_>>();
			assert_eq!(outputs.len(), 1);
			assert_eq!(outputs[0].script_pubkey, operator_script);
			assert_eq!(outputs[0].value, gross);
			operator_total += gross;
			assert_eq!(child.version.0, 3);
			assert!(child.vsize() <= 1_000);
			assert_eq!(child.input.len(), 2);
			let inputs = child.input.iter().map(|input| input.previous_output).collect::<BTreeSet<_>>();
			assert_eq!(inputs.len(), 2);
			assert!(inputs.contains(&anchor));
			let fee_point = *inputs.iter().find(|point| **point != anchor).unwrap();
			assert!(available.contains_key(&fee_point));
			assert!(spent_fees.insert(fee_point));
			let fee = watchman_fee(&ctx, child);
			assert!(fee > Amount::ZERO);
			assert_eq!(fee.to_sat(), u64::try_from(*observed_fee).unwrap());
			miner_total += fee;
			assert!(!child.output.is_empty() && child.output.iter().all(|output| output.value > Amount::ZERO));
			assert_eq!(available[&fee_point].value, fee + child.output.iter().map(|output| output.value).sum::<Amount>());
			for (index, output) in child.output.iter().enumerate() {
				assert!(new_change.insert(OutPoint::new(child.compute_txid(), index as u32), output.clone()).is_none());
			}
			assert!(all_packages.insert(*allocation, (parent.clone(), child.clone())).is_none());
		}
		assert_eq!(spent_roots, roots.keys().copied().collect::<BTreeSet<_>>());
		for point in &spent_fees { available.remove(point).unwrap(); }
		for (point, output) in new_change { assert!(available.insert(point, output).is_none()); }
		assert_eq!(sat(1_201_320), operator_total + miner_total
			+ available.values().map(|output| output.value).sum::<Amount>() + if late { Amount::ZERO } else { sat(1_320) });
		ctx.generate_blocks(6).await;
		for (parent, child, _) in packages.values() {
			assert_canonical_watchman_tx(&ctx, parent);
			assert_canonical_watchman_tx(&ctx, child);
		}
		for point in spent_roots.iter().chain(spent_fees.iter()) {
			assert!(core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().is_none());
		}
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let owned = watchman_confirmed_outpoints(wm).await;
				if available.keys().all(|point| owned.contains(point)) { break; }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("all positive change and unused fee coins must be confirmed watchman property");
		for (point, output) in &available {
			let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations >= 6);
			assert_eq!(actual.value, output.value);
			assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
		}
		if !late {
			assert!(ctx.bitcoind().get_block_count().await < u64::from(connector_expiry));
			let actual = core.get_tx_out(&connector_point.txid, connector_point.vout, Some(true)).unwrap().unwrap();
			assert_eq!(actual.value, sat(1_320));
			assert_eq!(actual.script_pub_key.script().unwrap(), offboard.output[1].script_pubkey);
			assert!(!db.read(async |tx| tx.get_bare_vtxo_by_id(connector_id).await).await.unwrap().decided_for_fallback);
			let rows = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM expiry_allocation_input WHERE vtxo_id=$1", &[&connector_id.to_string()]).await?)).await.unwrap();
			assert!(rows.is_empty(), "connector must have no early entitlement or allocation");
		}
	}
	assert_eq!(operator_total, sat(801_320));
	let mut delivered = BTreeMap::new();
	for height in scan_from..=ctx.bitcoind().get_block_count().await {
		for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
			for (index, output) in tx.output.iter().enumerate() {
				assert_ne!(output.script_pubkey, destination, "offboarded customer principal must not be refunded again");
				if output.script_pubkey == payout.script_pubkey() {
					delivered.insert(OutPoint::new(tx.compute_txid(), index as u32), output.value);
				}
			}
		}
	}
	assert_eq!(delivered, BTreeMap::from([(OutPoint::new(offboard_txid, 0), offboard.output[0].value)]));
	assert_canonical_watchman_tx(&ctx, &offboard);
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_sweeps_exit_after_oor_then_forfeit() {
	require_bark_version!(> "0.5.0");
	let ctx = TestContext::new("server/payouts/watchman_sweeps_exit_after_oor_then_forfeit").await;
	let srv = ctx.captaind("server").funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxopool.vtxo_targets = vec![];
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());
	// Preserve100k capital: one progress sponsor and seven independent current-root sponsors.
	assert!(sat(12_500) >= wm.config().watchman.min_cpfp_amount);
	let fee_address = wm.wait_wallet_address().await;
	let mut sponsors = BTreeMap::new();
	for _ in 0..8 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(12_500)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(12_500)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32), outputs[0].1.clone()).is_none());
	}
	assert_eq!(sponsors.values().map(|output| output.value).sum::<Amount>(), sat(100_000));
	let bark1 = ctx.bark("bark1", &srv).funded(sat(500_000)).create().await;
	let bark2 = ctx.bark("bark2", &srv).funded(sat(500_000)).create().await;
	bark1.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	bark2.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let mut originals = Vec::new();
	let mut destinations = Vec::new();
	for bark in [&bark1, &bark2] {
		let wallet = bark.client().await;
		let ids = bark.vtxo_ids().await;
		assert_eq!(ids.len(), 1);
		let original = wallet.get_full_vtxo(ids[0]).await.unwrap();
		assert_eq!(original.amount(), sat(200_000));
		originals.push(original);
		destinations.push(wallet.fallback_script());
	}
	ctx.refresh_all(&srv, &[&bark1, &bark2]).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	bark1.sync().await;
	bark2.sync().await;
	let stale = bark1.client().await.get_full_vtxo(bark1.vtxo_ids().await[0]).await.unwrap();
	let recipient = bark2.client().await.get_full_vtxo(bark2.vtxo_ids().await[0]).await.unwrap();
	assert_eq!(stale.amount(), sat(200_000));
	assert_eq!(recipient.amount(), sat(200_000));
	assert_eq!(stale.chain_anchor(), recipient.chain_anchor());
	let first_funding = ctx.bitcoind().await_transaction(stale.chain_anchor().txid).await;
	assert_eq!(first_funding.output[stale.chain_anchor().vout as usize].value, sat(400_000));
	for coin in [&stale, &recipient] { coin.validate(&first_funding).unwrap(); }

	let bark1_old = bark1.full_clone("bark1_old").await;
	bark1.send_oor(bark2.address().await, sat(50_000)).await;
	bark2.sync().await;
	let sender = bark1.client().await;
	let mut changes = Vec::new();
	for coin in sender.vtxos().await.unwrap() { changes.push(sender.get_full_vtxo(coin.id()).await.unwrap()); }
	assert!(!changes.is_empty());
	assert_eq!(changes.iter().map(|coin| coin.amount()).sum::<Amount>(), sat(150_000));
	drop(sender);
	let receiver = bark2.client().await;
	let received = receiver.vtxos().await.unwrap().into_iter().filter(|coin| coin.id() != recipient.id()).collect::<Vec<_>>();
	assert_eq!(received.len(), 1);
	let received = receiver.get_full_vtxo(received[0].id()).await.unwrap();
	assert_eq!(received.amount(), sat(50_000));
	drop(receiver);
	ctx.refresh_all(&srv, &[&bark2]).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	bark2.sync().await;
	let ids = bark2.vtxo_ids().await;
	assert_eq!(ids.len(), 1);
	let replacement = bark2.client().await.get_full_vtxo(ids[0]).await.unwrap();
	assert_eq!(replacement.amount(), sat(250_000));
	// The singleton round also has one operator padding leaf. Bind its signed backing.
	let round = db.read(async |tx| tx.get_round(replacement.chain_anchor().txid.into()).await).await.unwrap().unwrap();
	assert_eq!(round.funding_txid.as_round_txid(), replacement.chain_anchor().txid);
	let tree = round.into_cached_tree().unwrap();
	let outputs = tree.output_vtxos().collect::<Vec<_>>();
	assert_eq!(outputs.len(), 2);
	assert_eq!(outputs.iter().filter(|coin| coin.id() == replacement.id()).count(), 1);
	let padding = outputs.into_iter().find(|coin| coin.id() != replacement.id()).unwrap();
	assert_eq!(padding.amount(), sat(330));
	assert_eq!(padding.user_pubkey().to_string(), "02dfa52f6690299d2d6a08323083e290597b56fee125063e5f4e2957731639c42c");
	assert_eq!(padding.unlock_hash().unwrap().to_string(), "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
	assert_eq!(padding.chain_anchor(), replacement.chain_anchor());
	assert_eq!(padding.expiry_height(), replacement.expiry_height());
	let mut backing = tree.internal_vtxos().filter(|(coin, _)| matches!(coin.policy(),
		ark::ServerVtxoPolicy::HarkLeaf(policy) if policy.user_pubkey == padding.user_pubkey()
			&& Some(policy.unlock_hash) == padding.unlock_hash())).collect::<Vec<_>>();
	assert_eq!(backing.len(), 1);
	let (padding_backing, leaf_txid) = backing.remove(0);
	assert_eq!(leaf_txid, padding.point().txid);
	assert_eq!(padding_backing.amount(), padding.amount());
	assert_eq!(padding_backing.chain_anchor(), padding.chain_anchor());
	assert_eq!(padding_backing.expiry_height(), padding.expiry_height());
	let leaf = padding.transactions().last().unwrap().tx;
	assert_eq!(leaf.input.len(), 1);
	assert_eq!(leaf.input[0].previous_output, padding_backing.point());
	assert_eq!(leaf.output[padding.point().vout as usize], padding.txout());
	let stored = db.read(async |tx| tx.get_bare_vtxo_by_id(padding_backing.id()).await).await.unwrap();
	assert_eq!(stored.vtxo.txout(), padding_backing.txout());
	assert_eq!(stored.vtxo.policy(), padding_backing.policy());
	assert_eq!(stored.vtxo.chain_anchor(), padding.chain_anchor());
	assert_eq!(stored.oor_spent_txid, Some(leaf_txid));
	let stored = db.read(async |tx| tx.get_bare_vtxo_by_id(padding.id()).await).await.unwrap();
	assert_eq!(stored.spend_state, server::database::SpendState::Unclaimed);
	assert_eq!(stored.vtxo.txout(), padding.txout());
	assert_eq!(stored.vtxo.chain_anchor(), padding.chain_anchor());
	let mut fundings = BTreeMap::new();
	for coin in originals.iter().chain([&stale, &recipient, &received, &replacement]).chain(changes.iter()) {
		let funding = ctx.bitcoind().await_transaction(coin.chain_anchor().txid).await;
		coin.validate(&funding).unwrap();
		if let Some(previous) = fundings.insert(coin.chain_anchor().txid, funding.clone()) { assert_eq!(previous, funding); }
	}
	padding_backing.validate(&fundings[&padding.chain_anchor().txid]).unwrap();
	padding.validate_unsigned(&fundings[&padding.chain_anchor().txid]).unwrap();
	let backing_anchors = originals.iter().chain([&stale, &replacement]).map(|coin| coin.chain_anchor().to_string())
		.collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
	let core = ctx.bitcoind().sync_client();
	let scan_from = ctx.bitcoind().get_block_count().await + 1;
	bark1_old.start_exit_all().await;
	progress_exit_until_awaiting_delta(&ctx, &bark1_old).await;
	let tip = ctx.generate_blocks(wm.config().watchman.progress_grace_period.to_u32()).await;
	wm.wait_for_sync_height(tip).await;
	let mut log_progress = wm.subscribe_log::<ProgressBroadcast>();
	let (progress_tx, progress_child) = tokio::time::timeout(Duration::from_secs(10), async {
		wm.trigger_sweep().await;
		let progress = log_progress.recv().await.expect("real OOR progress");
		ctx.await_transactions_across_nodes([progress.cpfp_txid], [wm.bitcoind().as_ref()]).await;
		let parent = core.get_raw_transaction(&progress.txid, None).unwrap();
		assert_eq!(parent.input.len(), 1);
		assert_eq!(parent.input[0].previous_output, stale.point());
		assert!(changes.iter().chain([&received]).flat_map(|coin| coin.transactions()).any(|step| step.tx == parent));
		assert_eq!(watchman_fee(&ctx, &parent), Amount::ZERO);
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor()).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		let anchor = OutPoint::new(parent.compute_txid(), anchors[0].0 as u32);
		let child = core.get_raw_transaction(&progress.cpfp_txid, None).unwrap();
		assert_eq!(child.input.len(), 2);
		assert!(child.input.iter().any(|input| input.previous_output == anchor));
		assert_eq!(child.input.iter().filter(|input| sponsors.contains_key(&input.previous_output)).count(), 1);
		assert!(watchman_fee(&ctx, &child) > Amount::ZERO);
		(parent, child)
	}).await.expect("real progress and its funded Core child must appear within the original ten seconds");
	let tip = ctx.generate_blocks(srv.config().vtxo_lifetime.to_u32() - 2 * ROUND_CONFIRMATIONS + 3).await;
	let operator_script = wm.config().sweep_address.clone().unwrap().require_network(Network::Regtest).unwrap().script_pubkey();
	assert!(destinations.iter().all(|destination| destination != &operator_script));
	assert_ne!(destinations[0], destinations[1]);
	let replacement_mature = replacement.expiry_height() <= tip;
	let (packages, roots, expected, available) = tokio::time::timeout(Duration::from_secs(10), async {
		// All new attribution, the current view and publication share the original clock.
		for (coin, round) in originals.iter().map(|coin| (coin, stale.chain_anchor().txid))
			.chain([(&recipient, replacement.chain_anchor().txid), (&received, replacement.chain_anchor().txid)]) {
			let rows = db.read(async |tx| Ok(tx.query("SELECT p.id FROM round_part_input i \
				JOIN round_participation p ON p.id=i.participation_id JOIN round r ON r.funding_txid=p.round_id \
				JOIN vtxo v ON v.spent_in_round=r.id WHERE i.vtxo_id=$1 AND v.vtxo_id=$1 \
				AND i.signed_forfeit_tx IS NOT NULL AND p.forfeited_at IS NOT NULL AND r.funding_txid=$2",
				&[&coin.id().to_string(), &round.to_string()]).await?)).await.unwrap();
			assert_eq!(rows.len(), 1, "bind each operator right to its actual accepted round");
		}
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		assert_eq!(core.get_block_count().unwrap(), u64::from(tip.to_u32()));
		let hash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
		loop {
			if db.read(async |tx| {
				let Some(view) = tx.expiry_view().await? else { return Ok(false); };
				Ok(view.height == tip
					&& view.hash == hash && (tx.expiry_view().await? == Some(view)))
			}).await.unwrap() { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		let rows = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM vtxo WHERE anchor_point=ANY($1) \
			AND frontier_at IS NOT NULL AND confirmed_height<=$2 AND onchain_spent_height IS NULL AND expiry<=$2",
			&[&backing_anchors, &(tip.to_u32() as i32)]).await?)).await.unwrap();
		let mut roots = Vec::new();
		for row in rows {
			let id = ark::VtxoId::from_str(row.get("vtxo_id")).unwrap();
			let root = db.read(async |tx| tx.get_bare_vtxo_by_id(id).await).await.unwrap().vtxo;
			if let Some(actual) = core.get_tx_out(&root.point().txid, root.point().vout, Some(true)).unwrap() {
				assert!(actual.confirmations >= 1);
				assert_eq!(actual.value, root.amount());
				assert_eq!(actual.script_pub_key.script().unwrap(), root.output_script_pubkey());
				roots.push(root);
			}
		}
		assert!(!roots.is_empty());
		assert!(roots.iter().map(|root| root.expiry_height()).collect::<BTreeSet<_>>().len() <= 4);
		let mut semantic = originals.iter().chain([&recipient, &received]).cloned()
			.map(|coin| (ark::ServerVtxo::from(coin), None)).collect::<Vec<_>>();
		semantic.extend(changes.iter().cloned().map(|coin| (ark::ServerVtxo::from(coin), Some(0usize))));
		if replacement_mature {
			semantic.push((ark::ServerVtxo::from(replacement.clone()), Some(1)));
			semantic.push((ark::ServerVtxo::from(padding.clone()), None));
		}
		let mut expected = BTreeMap::new();
		for (mut coin, owner) in semantic {
			assert!(coin.expiry_height() <= tip);
			if roots.iter().find(|root| root.point() == coin.chain_anchor() || coin.transactions().any(|step| bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32) == root.point())).map(|root| root.id()).is_none() {
				assert!(owner.is_none(), "a rightful customer leaf cannot disappear");
				let state = db.read(async |tx| tx.get_bare_vtxo_by_id(coin.id()).await).await.unwrap();
				let id = ark::VtxoId::from(OutPoint::new(state.oor_spent_txid.expect("actual signed round forfeit"), 0));
				let descendant = db.read(async |tx| tx.get_server_vtxo_by_id(id).await).await.unwrap().vtxo;
				assert!(matches!(descendant.policy(), ark::ServerVtxoPolicy::HarkForfeit(_)));
				assert_eq!(descendant.amount(), coin.amount());
				assert!(descendant.transactions().any(|step| OutPoint::new(step.tx.compute_txid(), step.output_idx as u32) == coin.point()));
				coin = descendant;
			}
			let funding = &fundings[&coin.chain_anchor().txid];
			// Padding's unsigned leaf has no known preimage. Cover its signed current HarkLeaf instead.
			let terminal = if coin.id() == padding.id() { padding_backing.clone() } else { coin.clone() };
			terminal.validate(funding).unwrap();
			let root_id = roots.iter().find(|root| root.point() == terminal.chain_anchor() || terminal.transactions().any(|step| bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32) == root.point())).map(|root| root.id()).expect("every selected terminal needs one actual current root");
			let root = roots.iter().find(|root| root.id() == root_id).unwrap();
			assert!(expected.insert(coin.id().to_string(), (coin.clone(), owner, root.point())).is_none());
		}
		assert_eq!(expected.values().filter(|(coin, owner, _)| owner.is_none() && coin.id() != padding.id())
			.map(|(coin, _, _)| coin.amount()).sum::<Amount>(), sat(650_000));
		assert_eq!(expected.values().filter(|(coin, _, _)| coin.id() == padding.id())
			.map(|(coin, _, _)| coin.amount()).sum::<Amount>(), if replacement_mature { sat(330) } else { Amount::ZERO });
		assert_eq!(roots.iter().map(|root| root.amount()).sum::<Amount>(), expected.values().map(|(coin, _, _)| coin.amount()).sum::<Amount>());
		let consumed = progress_child.input.iter().filter(|input| sponsors.contains_key(&input.previous_output)).map(|input| input.previous_output).collect::<BTreeSet<_>>();
		let mut available = sponsors.iter().filter(|(point, _)| !consumed.contains(point)).map(|(point, output)| (*point, output.clone())).collect::<BTreeMap<_, _>>();
		for (index, output) in progress_child.output.iter().enumerate() {
			assert!(output.value > Amount::ZERO);
			assert!(available.insert(OutPoint::new(progress_child.compute_txid(), index as u32), output.clone()).is_none());
		}
		assert!(sponsors.keys().filter(|point| !consumed.contains(point)).count() >= roots.len(),
			"untouched confirmed fee inputs must cover one package per actual current root");
		loop {
			let owned = watchman_confirmed_outpoints(wm).await;
			if available.keys().all(|point| owned.contains(point)) { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		for (point, output) in &available {
			let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations >= 6);
			assert_eq!(actual.value, output.value);
			assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
		}
		if !replacement_mature {
			assert!(replacement.expiry_height() > tip);
			for id in [replacement.id(), padding.id()] {
				let count = db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) AS count FROM expiry_entitlement WHERE vtxo_id=$1",
					&[&id.to_string()]).await?.get::<_, i64>("count"))).await.unwrap();
				assert_eq!(count, 0, "the separate unexpired round outputs cannot be allocated early");
			}
		}
		let source_ids = expected.keys().cloned().collect::<Vec<_>>();
		// Captaind produces these entitlements independently; no trigger-created parent is a prerequisite.
		loop {
			let rows = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id,(SELECT amount FROM vtxo original WHERE original.vtxo_id=e.vtxo_id) AS gross_sat,(SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts FROM expiry_entitlement e WHERE e.vtxo_id=ANY($1)", &[&source_ids]).await?)).await.unwrap();
			if rows.len() == expected.len() {
				for row in rows {
					let (coin, _, _) = &expected[&row.get::<_, String>("vtxo_id")];
					assert_eq!(row.get::<_, i64>("gross_sat"), coin.amount().to_sat() as i64);
					let facts: serde_json::Value = serde_json::from_str(row.get("facts")).unwrap();
					if coin.id() == padding.id() { assert_eq!(facts["kind"], "padding"); }
				}
				if db.read(async |tx| {
					let Some(view) = tx.expiry_view().await? else { return Ok(false); };
					Ok(view.height == tip && view.hash == hash && (tx.expiry_view().await? == Some(view)))
				}).await.unwrap() { break; }
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		wm.trigger_sweep().await;
		let packages = wait_watchman_payouts(&ctx, &db, &source_ids).await;
		assert!(!packages.is_empty() && packages.len() <= roots.len(), "each package needs actual current backing");
		(packages, roots, expected, available)
	}).await.expect("actual current-cut funded payout packages must appear within the original ten seconds");
	let mut customer_gross_by_expiry = BTreeMap::<(usize, u32), u64>::new();
	for (coin, owner, _) in expected.values() {
		if let Some(owner) = owner {
			*customer_gross_by_expiry.entry((*owner, coin.expiry_height().to_u32())).or_default() += coin.amount().to_sat();
		}
	}
	let expected_fees_by_expiry = customer_gross_by_expiry.iter().map(|(group, gross)|
		(*group, gross.div_ceil(100).max(1_000))).collect::<BTreeMap<_, _>>();
	let mut parent_fees = BTreeMap::new();
	let tip = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(tip).await;
	wm.wait_for_sync_height(tip).await;
	assert_canonical_watchman_tx(&ctx, &progress_tx);
	assert_canonical_watchman_tx(&ctx, &progress_child);
	assert_eq!(core.custom_get_raw_transaction_info(progress_tx.compute_txid(), None).unwrap().unwrap().blockhash,
		core.custom_get_raw_transaction_info(progress_child.compute_txid(), None).unwrap().unwrap().blockhash);
	let mut spent_roots = BTreeSet::new();
	let mut expected_outputs = BTreeMap::new();
	let mut operator = Amount::ZERO;
	for (allocation, (parent, child, _observed_fee)) in &packages {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
		assert_eq!(core.custom_get_raw_transaction_info(parent.compute_txid(), None).unwrap().unwrap().blockhash,
			core.custom_get_raw_transaction_info(child.compute_txid(), None).unwrap().unwrap().blockhash);
		assert_eq!(parent.version.0, 3);
		assert!(parent.vsize() <= 10_000);
		assert_eq!(watchman_fee(&ctx, parent), Amount::ZERO);
		let mut gross = [Amount::ZERO; 2];
		let mut operator_gross = Amount::ZERO;
		for input in &parent.input {
			let point = input.previous_output;
			assert!(roots.iter().any(|root| root.point() == point));
			assert!(spent_roots.insert(point));
			for (coin, owner, _) in expected.values().filter(|(_, _, root)| *root == point) {
				if let Some(owner) = owner { gross[*owner] += coin.amount(); } else { operator_gross += coin.amount(); }
			}
		}
		let mut wanted = BTreeMap::new();
		for (owner, amount) in gross.into_iter().enumerate().filter(|(_, amount)| *amount > Amount::ZERO) {
			let outputs = parent.output.iter().filter(|output| output.script_pubkey == destinations[owner]).collect::<Vec<_>>();
			assert_eq!(outputs.len(), 1);
			let fee = amount.checked_sub(outputs[0].value).unwrap();
			assert!(fee.to_sat() <= amount.to_sat().div_ceil(100).max(1_000));
			assert!(parent_fees.insert((*allocation, owner), fee).is_none());
			wanted.insert(destinations[owner].clone(), amount - fee);
			operator_gross += fee;
		}
		wanted.insert(operator_script.clone(), operator_gross);
		let outputs = parent.output.iter().filter(|output| !output.is_p2a_fee_anchor()).map(|output| (output.script_pubkey.clone(), output.value)).collect::<BTreeMap<_, _>>();
		assert_eq!(outputs, wanted);
		assert_eq!(outputs.len() + 1, parent.output.len());
		operator += operator_gross;
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor()).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		let anchor = OutPoint::new(parent.compute_txid(), anchors[0].0 as u32);

		for (index, output) in parent.output.iter().enumerate() {
			let point = OutPoint::new(parent.compute_txid(), index as u32);
			if output.is_p2a_fee_anchor() { assert!(core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().is_none()); }
			else { expected_outputs.insert(point, output.clone()); }
		}
	}
	assert_eq!(spent_roots, roots.iter().map(|root| root.point()).collect());
	assert!(spent_roots.iter().any(|point| point.txid == progress_tx.compute_txid()),
		"the actual progressed checkpoint must be consumed by its authorized financial cut");
	let mut observed = BTreeMap::new();
	for height in scan_from..=ctx.bitcoind().get_block_count().await {
		for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
			for (index, output) in tx.output.iter().enumerate().filter(|(_, output)| destinations.iter().any(|destination| destination == &output.script_pubkey)) {
				observed.insert(OutPoint::new(tx.compute_txid(), index as u32), output.clone());
			}
		}
	}
	let wanted = expected_outputs.iter().filter(|(_, output)| destinations.iter().any(|destination| destination == &output.script_pubkey))
		.map(|(point, output)| (*point, output.clone())).collect::<BTreeMap<_, _>>();
	for (owner, destination) in destinations.iter().enumerate() {
		let gross = customer_gross_by_expiry.iter().filter(|((index, _), _)| *index == owner)
			.map(|(_, gross)| *gross).sum::<u64>();
		let fees = expected_fees_by_expiry.iter().filter(|((index, _), _)| *index == owner)
			.map(|(_, fee)| *fee).sum::<u64>();
		assert_eq!(wanted.values().filter(|output| output.script_pubkey == *destination)
			.map(|output| output.value).sum::<Amount>(), sat(gross - fees));
	}
	assert_eq!(observed, wanted, "all rightful outputs once; no spent ancestor or stale attacker refund");
	if !replacement_mature {
		let wallet = bark2.client().await;
		assert!(wallet.vtxos().await.unwrap().iter().any(|coin| coin.id() == replacement.id()));
		assert!(observed.values().all(|output| output.script_pubkey != destinations[1]));
	}
	failures.assert_empty();
}

#[tokio::test]
pub(super) async fn watchman_doesnt_broadcast_htlc_recv_chain() {
	let ctx = TestContext::new("server/payouts/watchman_doesnt_broadcast_htlc_recv_chain").await;
	let ln = ctx.new_lightning_setup("ln").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).lightningd(&ln.internal).funded(btc(10)).cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(144);
		// The pool never issues a single vtxo at a time, so ask for the minimum of two.
		cfg.vtxopool.vtxo_targets = vec![VtxoTarget { count: 2, amount: sat(100_000) }];
		cfg.vtxopool.vtxo_lifetime = BlockDelta::new(144);
		// Keep the change in the pool so all the receives share one allocation chain.
		cfg.vtxopool.max_vtxo_exit_depth = 100;
	}).watchmand_cfg(|cfg| {
		cfg.watchman.reaction_interval = Duration::from_secs(15 * 60);
		cfg.watchman.sweep_interval = Duration::from_secs(15 * 60);
	}).create().await;
	let failures = WatchmanFailureCollector::default();
	let wm = srv.watchmand();
	wm.add_slog_handler(failures.clone());
	let mut log_progress = wm.subscribe_log::<ProgressBroadcast>();

	// Keep the original one-million-sat budget, with one fee input for each original pool output.
	let fee_address = wm.wait_wallet_address().await;
	let mut sponsors = BTreeMap::new();
	for _ in 0..2 {
		let id = ctx.bitcoind().fund_addr(fee_address.clone(), sat(500_000)).await;
		let tx = ctx.bitcoind().await_transaction(id).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(500_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert!(sponsors.insert(OutPoint::new(id, outputs[0].0 as u32), outputs[0].1.clone()).is_none());
	}

	// Only the explicit oldest-coin exit may progress across the expiry jump.
	let wallet = ctx.bark_sdk("bark", &srv).cfg(|cfg| cfg.daemon_manual_sync = true)
		.funded(sat(1_000_000)).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let initial = futures::TryStreamExt::try_collect::<Vec<_>>(db.load_vtxopool().await.unwrap()).await.unwrap();
	assert_eq!(initial.len(), 2);
	assert!(initial.iter().all(|coin| coin.amount() == sat(100_000)));
	let anchor = initial[0].chain_anchor();
	assert!(initial.iter().all(|coin| coin.chain_anchor() == anchor));
	let funding = ctx.bitcoind().await_transaction(anchor.txid).await;
	assert_eq!(funding.output[anchor.vout as usize].value, sat(200_000));
	for coin in &initial { coin.validate(&funding).unwrap(); }
	let destination = wallet.fallback_script();
	let mut hashes = Vec::new();

	// Receive sequentially so each allocation spends the change of the previous one.
	for _ in 0..20 {
		let invoice = wallet.bolt11_invoice(sat(1_000), None, None).await
			.expect("creating the invoice");
		hashes.push(PaymentHash::from(&invoice).to_string());
		let (pay, _) = tokio::join!(
			ln.external.try_pay_bolt11(invoice.to_string()),
			wallet.try_claim_lightning_receive(PaymentHash::from(&invoice), true),
		);
		pay.expect("the lightning payment should succeed");
	}
	let mut customers = BTreeMap::new();
	for coin in wallet.vtxos().await.unwrap() {
		let full = wallet.get_full_vtxo(coin.id()).await.unwrap();
		assert_eq!(full.chain_anchor(), anchor);
		assert!(matches!(full.policy(), ark::VtxoPolicy::Pubkey(_)));
		full.validate(&funding).unwrap();
		assert!(customers.insert(full.id().to_string(), full).is_none());
	}
	assert_eq!(customers.len(), 20);
	let remainder = futures::TryStreamExt::try_collect::<Vec<_>>(db.load_vtxopool().await.unwrap()).await.unwrap();
	assert!(remainder.iter().all(|coin| coin.chain_anchor() == anchor));
	let mut operators = BTreeMap::new();
	for coin in remainder {
		coin.validate(&funding).unwrap();
		let stored = db.read(async |tx| tx.get_user_vtxo_by_id(coin.id()).await).await.unwrap();
		assert_eq!(stored.spend_state, server::database::SpendState::Pool);
		assert!(stored.oor_spent_txid.is_none());
		assert!(operators.insert(coin.id().to_string(), coin.inner().clone()).is_none());
	}
	assert_eq!(customers.values().map(|coin| coin.amount()).sum::<Amount>()
		+ operators.values().map(|coin| coin.amount()).sum::<Amount>(), sat(200_000));
	let claims = db.read(async |tx| Ok(tx.query("SELECT v.vtxo_id AS source_vtxo_id,h.payment_hash FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id \
		JOIN lightning_htlc_subscription s ON s.id=v.lightning_htlc_subscription_id \
		WHERE h.payment_hash=ANY($1) AND s.status='settled' AND h.direction='outgoing' AND h.offchain_resolution='fulfilled' ", &[&hashes]).await?)).await.unwrap();
	let mut grants = BTreeSet::new();
	let mut converted = BTreeSet::new();
	let mut settled = BTreeSet::new();
	for row in claims {
		let hash = row.get::<_, String>("payment_hash");
		assert!(hashes.contains(&hash));
		settled.insert(hash.clone());
		let source = row.get::<_, String>("source_vtxo_id");
		assert!(grants.insert(source.clone()));
		let grant = db.read(async |tx| tx.get_user_vtxo_by_id(ark::VtxoId::from_str(&source).unwrap()).await).await.unwrap().vtxo;
		assert_eq!(grant.chain_anchor(), anchor);
		assert_eq!(grant.policy().as_server_htlc_recv().unwrap().payment_hash.to_string(), hash);
		grant.validate(&funding).unwrap();
		let outputs = customers.iter().filter(|(_, coin)| coin.transactions().any(|step|
			step.tx.input.iter().any(|input| input.previous_output == grant.point()))).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1, "each fulfilled grant must have one authenticated signed recipient output");
		let (id, actual) = outputs[0];
		assert!(converted.insert(id.clone()));
		assert_eq!(grant.amount(), actual.amount());
		let registered = db.read(async |tx| tx.get_user_vtxo_by_id(actual.id()).await).await.unwrap();
		assert!(registered.oor_spent_txid.is_none());
		registered.vtxo.validate(&funding).unwrap();
	}
	assert_eq!(settled, hashes.iter().cloned().collect());
	assert_eq!(converted, customers.keys().cloned().collect());
	for coin in customers.values().chain(operators.values()) { for step in coin.transactions() {
		assert!(step.tx.output.iter().filter(|output| output.is_p2a_fee_anchor()).all(|output| output.value == Amount::ZERO));
	} }

	// The shortest exit chain belongs to the oldest receive. Exiting it drags the txs it
	// shares with the other receives on-chain.
	let oldest = wallet.vtxos().await.expect("listing vtxos").into_iter()
		.min_by_key(|v| v.exit_depth)
		.expect("there should be receives")
		.vtxo;
	wallet.exit_mgr().start_exit_for_vtxos(&[oldest.clone()]).await
		.expect("starting the exit");
	let mut state = None;
	for _ in 0..30 {
		wallet.sync_onchain().await.expect("onchain sync");
		wallet.progress_exits().await.expect("progressing the exit");
		state = wallet.exit_mgr().get_exit_vtxo(oldest.id()).await
			.map(|exit| exit.state().clone());
		if matches!(state, Some(ExitState::AwaitingDelta(_))) {
			break;
		}
		ctx.generate_blocks(1).await;
	}
	assert!(matches!(state, Some(ExitState::AwaitingDelta(_))),
		"the exit should have confirmed on-chain, was {:?}", state,
	);
	let claimable_height = state.as_ref().unwrap().claimable_height().unwrap();
	assert!(customers.contains_key(&oldest.id().to_string()));
	customers.remove(&oldest.id().to_string());
	assert_eq!(customers.len(), 19);
	assert!(customers.values().all(|coin| coin.amount() == sat(1_000)
		&& coin.expiry_height() == oldest.expiry_height()));
	assert_eq!(customers.values().map(|coin| coin.amount()).sum::<Amount>(), sat(19_000));

	let grace = u32::from(wm.config().watchman.progress_grace_period);
	let tip = ctx.generate_blocks(grace).await;
	wm.wait_for_sync_height(tip).await;

	// Give the watchman its chance to broadcast the receive chain; it must not take it.
	wm.trigger_sweep().await;
	assert!(log_progress.try_recv().is_err(), "the checkpoint must stop premature receive-chain progress");

	// Expiry pays the remaining customer rights and operator remainder through funded allocations.
	let core = ctx.bitcoind().sync_client();
	let scan_from = core.get_block_count().unwrap() + 1;
	let operator_script = wm.config().sweep_address.clone().unwrap().require_network(Network::Regtest).unwrap().script_pubkey();
	assert_ne!(operator_script, destination);
	let tip = ctx.generate_blocks(150).await;
	let (packages, roots, expected) = tokio::time::timeout(Duration::from_secs(30), async {
		srv.wait_for_sync_height(tip).await;
		wm.wait_for_sync_height(tip).await;
		let hash = core.get_block_hash(u64::from(tip.to_u32())).unwrap();
		let rows = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM vtxo WHERE anchor_point=$1 \
			AND frontier_at IS NOT NULL AND confirmed_height<=$2 AND onchain_spent_height IS NULL AND expiry<=$2",
			&[&anchor.to_string(), &(tip.to_u32() as i32)]).await?)).await.unwrap();
		let mut roots = Vec::new();
		for row in rows {
			let id = ark::VtxoId::from_str(row.get("vtxo_id")).unwrap();
			let root = db.read(async |tx| tx.get_bare_vtxo_by_id(id).await).await.unwrap().vtxo;
			if root.point() == oldest.point() { continue; }
			if let Some(actual) = core.get_tx_out(&root.point().txid, root.point().vout, Some(true)).unwrap() {
				assert!(actual.confirmations >= 6);
				assert_eq!(actual.value, root.amount());
				assert_eq!(actual.script_pub_key.script().unwrap(), root.output_script_pubkey());
				roots.push(root);
			}
		}
		assert!(!roots.is_empty() && roots.len() <= 2);
		assert_eq!(roots.iter().map(|root| root.amount()).sum::<Amount>() + oldest.amount(), sat(200_000));
		let mut expected = BTreeMap::new();
		for coin in customers.values().chain(operators.values()) {
			assert!(coin.expiry_height() <= tip);
			coin.validate(&funding).unwrap();
			let full = ark::ServerVtxo::from(coin.clone());
			let id = roots.iter().find(|root| root.point() == full.chain_anchor() || full.transactions().any(|step| bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32) == root.point())).map(|root| root.id()).expect("each remaining right needs actual current backing");
			let root = roots.iter().find(|root| root.id() == id).unwrap();
			assert!(expected.insert(coin.id().to_string(), (coin.clone(), root.point())).is_none());
		}
		let source_ids = expected.keys().cloned().collect::<Vec<_>>();
		loop {
			let allocated = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id FROM expiry_entitlement e \
				JOIN expiry_allocation_coin c ON c.entitlement_id=e.id WHERE e.vtxo_id=ANY($1) AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')",
				&[&source_ids]).await?.into_iter().map(|row| row.get::<_, String>("vtxo_id")).collect::<BTreeSet<_>>())).await.unwrap();
			let synced = db.read(async |tx| {
				let Some(view) = tx.expiry_view().await? else { return Ok(false); };
				Ok(view.height == tip && view.hash == hash)
			}).await.unwrap();
			let owned = watchman_confirmed_outpoints(wm).await;
			if allocated == source_ids.iter().cloned().collect() && synced && sponsors.keys().all(|point| owned.contains(point)) { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		for (point, output) in &sponsors {
			let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations >= 6);
			assert_eq!(actual.value, output.value);
			assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
		}
		wm.trigger_sweep().await;
		loop {
			let rows = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id,a.id,a.sweep_tx \
				FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
				JOIN expiry_allocation_state a ON a.id=c.allocation_id \
				WHERE e.vtxo_id=ANY($1) AND a.status<>'conflicted' AND a.broadcast_at IS NOT NULL", &[&source_ids]).await?)).await.unwrap();
			let mut seen = BTreeSet::new();
			let mut packages = BTreeMap::new();
			for row in rows {
				let parent = deserialize::<Transaction>(row.get("sweep_tx")).unwrap();
				let Some((child, measured_fee)) = observed_payout_fee_child(&ctx, &parent) else { continue; };
				assert!(seen.insert(row.get::<_, String>("vtxo_id")));
				let package = (parent, child, measured_fee);
				if let Some(previous) = packages.insert(row.get::<_, i64>("id"), package.clone()) { assert_eq!(previous, package); }
			}
			let rebuilt = db.read(async |tx| {
				let Some(view) = tx.expiry_view().await? else { return Ok(false); };
				Ok(view.height == tip && view.hash == hash && (tx.expiry_view().await? == Some(view)))
			}).await.unwrap();
			if rebuilt && seen == source_ids.iter().cloned().collect() && packages.values().all(|(parent, child, _)|
				core.get_raw_transaction(&parent.compute_txid(), None).ok().as_ref() == Some(parent)
				&& core.get_raw_transaction(&child.compute_txid(), None).ok().as_ref() == Some(child)) { break (packages, roots, expected); }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the complete current cut must publish real funded packages within the original30seconds");
	assert!(!packages.is_empty() && packages.len() <= 2);
	let mut consumed_roots = BTreeSet::new();
	let mut payouts = BTreeMap::new();
	let mut customer_paid = Amount::ZERO;
	let mut operator_paid = Amount::ZERO;
	for (parent, child, observed_fee) in packages.values() {
		assert_eq!(parent.version.0, 3);
		assert!(parent.vsize() <= 10_000);
		let mut gross = Amount::ZERO;
		let mut customer = Amount::ZERO;
		for input in &parent.input {
			let root = roots.iter().find(|root| root.point() == input.previous_output).unwrap();
			assert!(consumed_roots.insert(root.point()));
			gross += root.amount();
			customer += expected.values().filter(|(coin, point)| *point == root.point() && customers.contains_key(&coin.id().to_string()))
				.map(|(coin, _)| coin.amount()).sum::<Amount>();
		}
		assert_eq!(parent.output.iter().map(|output| output.value).sum::<Amount>(), gross);
		let mut wanted = BTreeMap::new();
		let fee = if customer > Amount::ZERO {
			let outputs = parent.output.iter().filter(|output| output.script_pubkey == destination).collect::<Vec<_>>();
			assert_eq!(outputs.len(), 1);
			let fee = customer.checked_sub(outputs[0].value).unwrap();
			assert!(fee == Amount::ZERO || fee == sat(1_000));
			fee
		} else { Amount::ZERO };
		if customer > Amount::ZERO { wanted.insert(destination.clone(), customer.checked_sub(fee).unwrap()); }
		wanted.insert(operator_script.clone(), gross.checked_sub(customer).unwrap() + fee);
		let outputs = parent.output.iter().filter(|output| !output.is_p2a_fee_anchor())
			.map(|output| (output.script_pubkey.clone(), output.value)).collect::<BTreeMap<_, _>>();
		assert_eq!(outputs, wanted);
		assert_eq!(outputs.len() + 1, parent.output.len());
		customer_paid += customer.checked_sub(fee).unwrap();
		operator_paid += gross.checked_sub(customer).unwrap() + fee;
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor()).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		assert_eq!(anchors[0].1.value, Amount::ZERO);
		let anchor = OutPoint::new(parent.compute_txid(), anchors[0].0 as u32);
		assert_eq!(child.version.0, 3);
		assert!(child.vsize() <= 1_000);

		for (index, output) in parent.output.iter().enumerate().filter(|(_, output)| !output.is_p2a_fee_anchor()) {
			assert!(payouts.insert(OutPoint::new(parent.compute_txid(), index as u32), output.clone()).is_none());
		}
	}
	assert_eq!(consumed_roots, roots.iter().map(|root| root.point()).collect());
	assert_eq!(customer_paid, sat(18_000));
	assert_eq!(operator_paid, sat(181_000));
	assert_eq!(sat(19_000), customer_paid + sat(1_000),
		"all 19 rights pay the 1,000 sat floor once across every allocation");
	let final_tip = ctx.generate_blocks(6).await;
	assert!(final_tip >= claimable_height);
	srv.wait_for_sync_height(final_tip).await;
	wm.wait_for_sync_height(final_tip).await;
	for (parent, child, _) in packages.values() {
		assert_canonical_watchman_tx(&ctx, parent);
		assert_canonical_watchman_tx(&ctx, child);
	}
	assert_canonical_watchman_tx(&ctx, &core.get_raw_transaction(&oldest.point().txid, None).unwrap());
	let exit = core.get_tx_out(&oldest.point().txid, oldest.point().vout, Some(true)).unwrap().unwrap();
	assert!(exit.confirmations >= 6);
	assert_eq!(exit.value, oldest.amount());
	assert_eq!(exit.script_pub_key.script().unwrap(), oldest.output_script_pubkey());
	let mut observed = BTreeMap::new();
	for height in scan_from..=core.get_block_count().unwrap() {
		for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
			for (index, output) in tx.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == destination) {
				observed.insert(OutPoint::new(tx.compute_txid(), index as u32), output.clone());
			}
		}
	}
	assert_eq!(observed, payouts.iter().filter(|(_, output)| output.script_pubkey == destination)
		.map(|(point, output)| (*point, output.clone())).collect(), "remaining rights paid once; no grant ancestor or native-exit refund");

	// The watchman broadcast none of the txs in the receive chain.
	if let Ok(p) = log_progress.try_recv() {
		panic!("watchman broadcast progress tx {} for vtxo {}", p.txid, p.vtxo_id);
	}

	failures.assert_empty();
	let customer_payouts = payouts.iter().filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(customer_payouts.len(), 1);
	assert_eq!(customer_payouts[0].1.value, sat(18_000));
	check_receive_seed_custody(&ctx, &srv, &wallet, &oldest, &destination, *customer_payouts[0].0, &customers).await;
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

pub(super) async fn watchman_confirmed_outpoints(watchman: &Watchmand) -> BTreeSet<OutPoint> {
	let address = watchman.config().admin_address.unwrap();
	let mut rpc = WalletAdminClient::connect(format!("http://{address}")).await.unwrap();
	rpc.wallet_status(protos::Empty {}).await.unwrap().into_inner().watchman.unwrap()
		.confirmed_utxos.into_iter().map(|point| OutPoint::from_str(&point).unwrap()).collect()
}

pub(super) async fn check_receive_seed_custody(
	ctx: &TestContext, srv: &ark_testing::Captaind,
	wallet: &bark::Wallet, oldest: &ark::Vtxo<ark::vtxo::Bare>, destination: &bitcoin::ScriptBuf,
	payout: OutPoint, remaining: &BTreeMap<String, ark::Vtxo>,
) {
	// Only the original seed and genesis birthday enter the new REST database.
	// Keep the seed in memory and origin files, outside argv and public receipts.
	let seed = bip39::Mnemonic::from_str(&std::fs::read_to_string(ctx.datadir.join("bark/mnemonic")).unwrap()).unwrap();
	let original_debug = bark_json::cli::DebugInfo::from(wallet.debug_info().await.unwrap());
	wallet.stop_daemon_wait().await.unwrap();
	let restored = ctx.barkd("seed_only", srv).mnemonic(seed.to_string()).birthday_height(0)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	restored.sync().await;
	restored.onchain_sync().await;
	restored.exit_progress().await;
	let utxos = restored.onchain_utxos().await;
	assert!(utxos.iter().any(|coin| coin.outpoint == payout && coin.amount == sat(18_000)),
		"the fresh seed must recognize the actual full-cohort fallback output");
	let recovered = restored.vtxos(Some(true)).await;
	for id in remaining.keys() {
		let coin = recovered.iter().find(|coin| coin.id.to_string() == *id)
			.expect("the fresh seed must retain every paid right");
		assert_eq!(coin.state, VtxoStateInfo::Spent, "a paid right must not become spendable during recovery");
	}
	let exits = restored.get_all_exit_status(Some(true), Some(false)).await;
	let oldest_state = exits.iter().find(|exit| exit.vtxo_id == oldest.id());
	let oldest_claimable = oldest_state.is_some_and(|exit| matches!(exit.state, bark_json::exit::ExitState::Claimable(_)));
	let oldest_row = recovered.iter().find(|coin| coin.id == oldest.id());
	if let Some(coin) = oldest_row {
		assert_eq!(coin.amount, sat(1_000));
		assert_eq!(coin.state, VtxoStateInfo::Exited);
	}

	// Read the stopped fresh database without a second recovery scan. Derive
	// public descriptors from the seed, and compare its recovered key.
	restored.stop().await.unwrap();
	let cfg = toml::from_str(&std::fs::read_to_string(restored.datadir().join("config.toml")).unwrap()).unwrap();
	let reader = bark::Wallet::open(Network::Regtest, bark::WalletSeed::new_from_mnemonic(Network::Regtest, &seed), cfg,
		bark::OpenWalletArgs { datadir: Some(restored.datadir()), run_daemon: false, skip_recovery: true,
			lock_manager: Some(Box::new(bark::lock_manager::memory::MemoryLockManager::new())), ..Default::default() }).await.unwrap();
	let debug = bark_json::cli::DebugInfo::from(reader.debug_info().await.unwrap());
	assert_eq!(serde_json::to_value(&debug).unwrap(), serde_json::to_value(&original_debug).unwrap());
	let fallback = reader.fallback_script();
	assert_eq!(&fallback, destination);

	let oldest_key_index = if oldest_row.is_some() {
		let signed = reader.get_full_vtxo(oldest.id()).await.unwrap();
		assert_eq!(signed.to_bare(), *oldest);
		let (index, _) = reader.pubkey_keypair(&signed.user_pubkey()).await.unwrap()
			.expect("the recovered oldest exit must have a seed-derived key");
		assert_eq!(reader.vtxo_xpub().derive_pub(&ark::SECP, &[index.into()]).unwrap().public_key, signed.user_pubkey());
		Some(index)
	} else { None };
	drop(reader);
	let db = Arc::new(bark::persist::sqlite::SqliteClient::open(restored.datadir().join("db.sqlite")).unwrap());
	let onchain = bark::onchain::OnchainWallet::load_or_create(Network::Regtest, seed.to_seed(""), db).await.unwrap();
	let master = bitcoin::bip32::Xpriv::new_master(Network::Regtest, &seed.to_seed("")).unwrap();
	let (descriptor, _private_keys, _) = bdk_wallet::template::DescriptorTemplate::build(
		bdk_wallet::template::Bip86(master, KeychainKind::External), Network::Regtest.into()).unwrap();
	assert_eq!(descriptor, *onchain.inner.public_descriptor(KeychainKind::External));
	drop(onchain);
	restored.start().await.unwrap();

	let mut oldest_claim = None;
	if oldest_claimable && oldest_key_index.is_some() {
		let address = restored.onchain_address().await;
		let response = bark_rest_client::apis::exits_api::exit_claim_vtxos(&restored.client_config(),
			bark_json::web::ExitClaimVtxosRequest { destination: address.to_string(),
				vtxos: vec![oldest.id().to_string()], fee_rate: None }).await;
		response.expect("fresh recovered oldest exit claim failed; inspect origin logs");
		let core = ctx.bitcoind().sync_client();
		let txid = tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				if let Some(txid) = core.get_mempool_spending_tx(oldest.point()).unwrap() { break txid; }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("fresh oldest claim did not reach the verifying node");
		let tx = ctx.bitcoind().await_transaction(txid).await;
		assert_eq!(tx.input.len(), 1);
		assert_eq!(tx.input[0].previous_output, oldest.point());
		assert_eq!(tx.output.len(), 1);
		assert_eq!(tx.output[0].script_pubkey, address.script_pubkey());
		assert!(tx.output[0].value > Amount::ZERO && tx.output[0].value < oldest.amount());
		ctx.generate_blocks(6).await;
		assert_canonical_watchman_tx(ctx, &tx);
		let claimed = core.get_tx_out(&txid, 0, Some(true)).unwrap().unwrap();
		assert!(claimed.confirmations >= 6);
		assert!(core.get_tx_out(&oldest.point().txid, oldest.point().vout, Some(true)).unwrap().is_none());
		oldest_claim = Some(OutPoint::new(txid, 0));
	}
	restored.onchain_sync().await;
	restored.exit_progress().await;
	let recovery_address = ctx.bitcoind().get_new_address();
	let drain = bark_rest_client::apis::onchain_api::onchain_drain(&restored.client_config(),
		bark_json::web::OnchainDrainRequest { destination: recovery_address.to_string() }).await.unwrap();
	let recovery = ctx.bitcoind().await_transaction(drain.txid).await;
	assert_eq!(recovery.input.iter().filter(|input| input.previous_output == payout).count(), 1);
	if let Some(point) = oldest_claim {
		assert_eq!(recovery.input.iter().filter(|input| input.previous_output == point).count(), 1);
	}
	let input_value = recovery.input.iter().map(|input| {
		let previous = ctx.bitcoind().sync_client().get_raw_transaction(&input.previous_output.txid, None).unwrap();
		previous.output[input.previous_output.vout as usize].value
	}).sum::<Amount>();
	let fee = input_value - recovery.output.iter().map(|output| output.value).sum::<Amount>();
	assert!(fee > Amount::ZERO);
	ctx.generate_blocks(6).await;
	assert_canonical_watchman_tx(ctx, &recovery);
	let recipients = recovery.output.iter().enumerate().filter(|(_, output)|
		output.script_pubkey == recovery_address.script_pubkey()).collect::<Vec<_>>();
	assert_eq!(recipients.len(), 1);
	let point = OutPoint::new(recovery.compute_txid(), recipients[0].0 as u32);
	let deep = ctx.bitcoind().sync_client().get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
	assert!(deep.confirmations >= 6);
	assert_eq!(deep.value, recipients[0].1.value);
	assert!(ctx.bitcoind().sync_client().get_tx_out(&payout.txid, payout.vout, Some(true)).unwrap().is_none());
	restored.onchain_sync().await;
	restored.exit_progress().await;
	restored.stop().await.unwrap();
}
