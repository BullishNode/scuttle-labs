//! A real terminal user claim and its backing conflict disappear after a reorg.

use super::expiry_test::{actual_fee, canonical as canonical_point, wait_rebuilt as wait_exact_rebuilt};

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use ark::ProtocolEncoding;
use ark::attestations::VtxoStatusAttestation;
use ark_testing::{require_bitcoind_chain_source, sat, Captaind, TestContext};
use ark_testing::constants::BOARD_CONFIRMATIONS;
use ark_testing::exit::{complete_exit, progress_exit_until_awaiting_delta};
use bitcoin::{Amount, BlockHash, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use bitcoin::consensus::deserialize;
use bitcoin_ext::{BlockDelta, BlockRef};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::{BlockTable, Db};
use server_rpc::protos;

/// Reconcile the winning direct payouts and their exact on-chain fees.
/// Losing allocations remain signed and immutable; neither their age nor an
/// attempted replacement determines the rightful canonical payment.
async fn assert_physical_cut(
	ctx: &TestContext, db: &Db, server: &Captaind, coins: &[String],
	expected_payers: &BTreeMap<String, Txid>, stored_parents: &BTreeMap<i64, Transaction>,
	canonical: &BTreeMap<Txid, BlockHash>, backing: &BTreeMap<OutPoint, TxOut>,
	sponsors: &BTreeMap<OutPoint, TxOut>, customer: &ScriptBuf,
	owner_mailbox: &str,
	initial_funding: &BTreeMap<OutPoint, TxOut>, boards: &BTreeMap<Txid, Transaction>,
	ordinary_change: &BTreeMap<OutPoint, TxOut>, board_fee: Amount,
) -> (BTreeMap<Txid, Transaction>, Amount, Amount) {
	let operator = {
		let config = server.watchmand().config();
		config.sweep_address.clone().unwrap().require_network(config.network).unwrap().script_pubkey()
	};
	assert_ne!(&operator, customer);
	let mut graph = BTreeMap::new();
	let mut parent_points = BTreeMap::new();
	let mut fee_total = Amount::ZERO;
	let mut customer_total = Amount::ZERO;
	for (txid, hash) in canonical {
		let parents = stored_parents.iter().filter(|(_, transaction)| transaction.compute_txid() == *txid).collect::<Vec<_>>();
		assert_eq!(parents.len(), 1);
		let (allocation, parent) = parents[0];
		let point = canonical_point(ctx, parent);
		assert_eq!(point.hash, *hash);
		parent_points.insert(*txid, point);
		assert_eq!(parent.version.0, 2);
		assert!(parent.vsize() <= 100_000);
		let miner_fee = actual_fee(ctx, parent).await;
		assert!(miner_fee > Amount::ZERO);
		fee_total += miner_fee;
		let payees = parent.output.iter().enumerate().filter(|(_, output)| &output.script_pubkey == customer).collect::<Vec<_>>();
		assert_eq!(payees.len(), 1);
		let (_, payee) = payees[0];
		customer_total += payee.value;
		let remainder = parent.output.iter().filter(|output| &output.script_pubkey != customer)
			.map(|output| output.value).sum::<Amount>();
		let parent_gross = parent.input.iter().filter_map(|input|
			backing.get(&input.previous_output)).map(|output| output.value).sum::<Amount>();
		let sponsor_gross = parent.input.iter().filter_map(|input|
			sponsors.get(&input.previous_output)).map(|output| output.value).sum::<Amount>();
		assert_eq!(payee.value + sat(2_000), parent_gross);
		assert_eq!(parent_gross + sponsor_gross, payee.value + remainder + miner_fee);
		for output in parent.output.iter().filter(|output| &output.script_pubkey != customer) {
			assert_eq!(output.script_pubkey, operator);
		}
		let inputs = db.read(async |tx| Ok(tx.query("SELECT vtxo_id FROM expiry_allocation_input WHERE allocation_id=$1", &[allocation]).await?)).await.unwrap();
		assert_eq!(inputs.into_iter().map(|row| row.get::<_, String>(0)).collect::<BTreeSet<_>>(),
			parent.input.iter().filter(|input| backing.contains_key(&input.previous_output))
				.map(|input| input.previous_output.to_string()).collect());
		assert!(graph.insert(*txid, parent.clone()).is_none());
	}
	let package_fee = fee_total;
	let mut actual_board_fee = Amount::ZERO;
	for (txid, board) in boards {
		assert_eq!(*txid, board.compute_txid());
		canonical_point(ctx, board);
		actual_board_fee += actual_fee(ctx, board).await;
		assert!(graph.insert(*txid, board.clone()).is_none());
	}
	assert_eq!(actual_board_fee, board_fee);
	fee_total += actual_board_fee;
	let rows = db.read(async |tx| Ok(tx.query("SELECT c.vtxo_id,(SELECT amount FROM vtxo original WHERE original.vtxo_id=c.vtxo_id) AS entitlement_gross,c.disposition,c.owner_mailbox, \
		c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat,a.sweep_txid,a.status,a.confirmed_height,a.confirmed_hash \
		FROM expiry_allocation_coin c \
		JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE a.status<>'conflicted' AND c.vtxo_id=ANY($1)", &[&coins]).await?)).await.unwrap();
	assert_eq!(rows.len(), coins.len());
	let mut attributed_payout = Amount::ZERO;
	let mut attributed_fee = Amount::ZERO;
	for row in rows {
		let coin = row.get::<_, String>("vtxo_id");
		let payer = expected_payers[&coin];
		let point = parent_points[&payer];
		assert_eq!(row.get::<_, String>("disposition"), "customer");
		assert_eq!(row.get::<_, String>("owner_mailbox"), owner_mailbox);
		assert_eq!(row.get::<_, i64>("gross_sat"), 100_000);
		assert_eq!(row.get::<_, i64>("entitlement_gross"), 100_000);
		assert_eq!(row.get::<_, i64>("forfeited_sat"), 0);
		assert_eq!(row.get::<_, String>("status"), "confirmed");
		assert_eq!(row.get::<_, Option<String>>("sweep_txid"), Some(payer.to_string()));
		assert_eq!(row.get::<_, Option<i64>>("confirmed_height"), Some(i64::from(point.height.to_u32())));
		assert_eq!(row.get::<_, Option<String>>("confirmed_hash"), Some(point.hash.to_string()));
		let payout = sat(u64::try_from(row.get::<_, i64>("payout_sat")).unwrap());
		let fee = sat(u64::try_from(row.get::<_, i64>("fee_sat")).unwrap());
		assert_eq!(payout + fee, sat(100_000));
		attributed_payout += payout;
		attributed_fee += fee;
	}
	assert_eq!(attributed_payout, customer_total);
	assert_eq!(attributed_fee, sat(2_000 * canonical.len() as u64));
	assert_eq!(graph.values().flat_map(|transaction| transaction.output.iter())
		.filter(|output| &output.script_pubkey == customer).count(), canonical.len(),
		"each canonical payout has exactly one aggregated customer output");
	let mut spent = BTreeSet::new();
	let mut external = BTreeSet::new();
	for transaction in graph.values() {
		for input in &transaction.input {
			assert!(spent.insert(input.previous_output), "a canonical money DAG cannot spend an outpoint twice");
			let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
			let output = &previous.output[input.previous_output.vout as usize];
			if let Some(internal) = graph.get(&input.previous_output.txid) {
				assert_eq!(output, &internal.output[input.previous_output.vout as usize]);
			} else {
				let declared = initial_funding.get(&input.previous_output).or_else(|| sponsors.get(&input.previous_output))
					.expect("only the actual initial customer funding and declared confirmed sponsors may enter the complete board/payout money DAG");
				assert_eq!(output, declared);
				external.insert(input.previous_output);
			}
		}
	}
	assert!(initial_funding.keys().all(|point| external.contains(point)));
	assert!(backing.keys().all(|point| spent.contains(point)));
	let mut retained = BTreeMap::new();
	let mut unspent_sponsors = BTreeSet::new();
	let mut customer_leaves = Amount::ZERO;
	let mut ordinary_leaves = BTreeMap::new();
	for (txid, transaction) in &graph {
		for (vout, output) in transaction.output.iter().enumerate() {
			let point = OutPoint::new(*txid, vout as u32);
			if spent.contains(&point) { continue; }
			let actual = ctx.bitcoind().sync_client().get_tx_out(txid, vout as u32, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations >= 6);
			assert_eq!(actual.value, output.value);
			assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
			if let Some(expected) = ordinary_change.get(&point) {
				assert_eq!(output, expected);
				assert!(ordinary_leaves.insert(point, output.clone()).is_none());
			} else if &output.script_pubkey == customer { customer_leaves += output.value; }
			else {
				assert!(!boards.contains_key(txid), "no unexplained surviving board output may disappear from customer accounting");
				if canonical.contains_key(txid) { assert_eq!(output.script_pubkey, operator); }
					else { panic!("unexpected non-board, non-payout retained output"); }
				retained.insert(point, output.value);
			}
		}
	}
	assert_eq!(customer_leaves, customer_total, "all rightful customer outputs must remain economically available");
	assert_eq!(&ordinary_leaves, ordinary_change,
		"the exact pre-exit ordinary board change must survive the whole conflicting branch rollback without reopening the old holder");
	for (point, output) in sponsors {
		if spent.contains(point) { continue; }
		let actual = ctx.bitcoind().sync_client().get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 6);
		assert_eq!(actual.value, output.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
		unspent_sponsors.insert(*point); // An unused declared sponsor must still be owned.
		assert!(retained.insert(*point, output.value).is_none());
	}
	tokio::time::timeout(Duration::from_secs(10), async {
		loop {
			let owned = super::expiry_test::watchman_coins(server.watchmand()).await;
			if unspent_sponsors.is_subset(&owned) { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("all unused sponsor funding must remain in the real watchman wallet");
	let operator_total = retained.values().copied().sum::<Amount>();
	let source_total = backing.values().map(|output| output.value).sum::<Amount>();
	let sponsor_total = sponsors.values().map(|output| output.value).sum::<Amount>();
	let initial_total = initial_funding.values().map(|output| output.value).sum::<Amount>();
	let ordinary_total = ordinary_change.values().map(|output| output.value).sum::<Amount>();
	assert_eq!(initial_total, sat(500_000));
	assert_eq!(sponsor_total, sat(200_000));
	assert_eq!(initial_total, source_total + ordinary_total + board_fee);
	assert_eq!(source_total, customer_total + attributed_fee);
	assert_eq!(source_total + sponsor_total, customer_total + operator_total + package_fee);
	assert_eq!(sponsor_total + attributed_fee, operator_total + package_fee,
		"customer payout charges and actual miner fees must remain separate and exactly conserved");
	assert_eq!(initial_total + sponsor_total, customer_total + ordinary_total + operator_total + fee_total,
		"the entire declared 700k initial customer-plus-sponsor funding must survive as exact customer/operator Core leaves and actual board/payout miner fees");
	(graph, fee_total, operator_total)
}

async fn decided_obligations(db: &Db, coins: &[String]) -> BTreeMap<String, (i64, i64, String)> {
	let rows = db.read(async |tx| Ok(tx.query("SELECT MIN(c.allocation_id) AS id,v.vtxo_id,v.amount AS gross_sat, \
		(SELECT replannable::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.vtxo_id=v.vtxo_id ORDER BY allocation_id LIMIT 1) AS facts,v.spend_state::TEXT AS spend_state \
		FROM expiry_allocation_coin c JOIN vtxo v ON v.vtxo_id=c.vtxo_id WHERE c.vtxo_id=ANY($1) GROUP BY v.vtxo_id,v.amount,v.spend_state",
		&[&coins],
	).await?)).await.unwrap();
	assert_eq!(rows.len(), coins.len(), "both committed obligations must survive every fork");
	rows.into_iter().map(|row| {
		assert_eq!(row.get::<_, String>("spend_state"), "spent", "reorg must never reopen a decided source");
		(row.get("vtxo_id"), (row.get("id"), row.get("gross_sat"), row.get("facts")))
	}).collect()
}

async fn rebuilt_generation(db: &Db, point: BlockRef) -> BlockRef {
	wait_exact_rebuilt(db, point, Duration::from_secs(30), "both actual daemon views must rebuild the current Core branch").await
}

async fn assert_final_outcome(
	ctx: &TestContext, db: &Db, coins: &[String], expected_payers: &BTreeMap<String, Txid>,
	stored_parents: &BTreeMap<i64, Transaction>, canonical: &BTreeMap<Txid, BlockHash>,
) {
	let rows = db.read(async |tx| Ok(tx.query("SELECT coin.vtxo_id, \
		(SELECT COUNT(*) FROM expiry_allocation_coin c WHERE c.vtxo_id=coin.vtxo_id AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')) AS active, \
		(SELECT COUNT(DISTINCT a.sweep_txid) FROM expiry_allocation_coin c \
		 JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE c.vtxo_id=coin.vtxo_id AND a.status='confirmed') AS payments \
		FROM (SELECT DISTINCT vtxo_id FROM expiry_allocation_coin WHERE vtxo_id=ANY($1)) coin", &[&coins],
	).await?)).await.unwrap();
	assert_eq!(rows.len(), 2);
	for row in rows {
		let coin = row.get::<_, String>("vtxo_id");
		assert_eq!(row.get::<_, i64>("active"), 1);
		assert_eq!(row.get::<_, i64>("payments"), 1);
		let active = db.read(async |tx| Ok(tx.query_one("SELECT a.sweep_txid FROM expiry_allocation_state a \
			JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
			WHERE c.vtxo_id=$1 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')", &[&coin],
		).await?.get::<_, String>(0))).await.unwrap();
		assert_eq!(active, expected_payers[&coin].to_string(), "the active receipt must identify the actual canonical payer");
	}
	let allocations = db.read(async |tx| Ok(tx.query("SELECT id,status,sweep_tx FROM expiry_allocation_state \
		WHERE id=ANY($1)", &[&stored_parents.keys().copied().collect::<Vec<_>>()],
	).await?)).await.unwrap();
	assert_eq!(allocations.len(), stored_parents.len());
	for row in allocations {
		let id: i64 = row.get("id");
		let parent = deserialize::<Transaction>(row.get("sweep_tx")).unwrap();
		assert_eq!(parent, stored_parents[&id]);
		assert_eq!(row.get::<_, String>("status"), if canonical.contains_key(&parent.compute_txid()) { "confirmed" } else { "conflicted" });
	}
	// Include any new allocation created after the first observation, so an
	// additional canonical payment cannot escape the immutable-parent oracle.
	let parents = db.read(async |tx| Ok(tx.query("SELECT DISTINCT a.sweep_txid \
		FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
		WHERE c.vtxo_id=ANY($1) AND a.sweep_txid IS NOT NULL", &[&coins],
	).await?)).await.unwrap();
	let rpc = ctx.bitcoind().sync_client();
	let all_canonical = parents.into_iter().filter_map(|row| {
		let txid = row.get::<_, String>("sweep_txid").parse::<Txid>().unwrap();
		rpc.custom_get_raw_transaction_info(txid, None).unwrap()
			.filter(|info| info.confirmations.unwrap_or(0) > 0).map(|_| txid)
	}).collect::<BTreeSet<_>>();
	assert_eq!(all_canonical, canonical.keys().copied().collect::<BTreeSet<_>>());
}

/// No synthetic financial rows or chain acknowledgments: only native boards,
/// signed holder exit/claim, real Core forks and owned restarts.
#[tokio::test]
async fn expiry_actual_partial_conflict_reorg_restores_one_canonical_payment() {
	require_bitcoind_chain_source!();
	tokio::time::timeout(Duration::from_secs(180), Box::pin(partial_conflict_reorg())).await
		.expect("the bounded native partial-conflict reorg case must finish");
}

async fn partial_conflict_reorg() {
	let ctx = TestContext::new("server/partial_conflict_reorg").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(24);
			cfg.vtxo_exit_delta = BlockDelta::new(3);
		}).watchmand().create().await;
	// Keep the watchman unfunded until the alternate branch. Direct payouts
	// must still pay their miner fee from the operator's retained charge.
	let fee_address = srv.watchmand().wait_wallet_address().await;
	let wallet = ctx.bark("holder", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let initial_txid = ctx.fund_bark(&wallet, sat(500_000)).await;
	let mut initial_funding = BTreeMap::new();
	for utxo in wallet.onchain_utxos().await {
		assert_eq!(utxo.outpoint.txid, initial_txid);
		let transaction = ctx.bitcoind().await_transaction(utxo.outpoint.txid).await;
		let output = transaction.output[utxo.outpoint.vout as usize].clone();
		assert_eq!(output.value, utxo.amount);
		assert!(initial_funding.insert(utxo.outpoint, output).is_none());
	}
	assert_eq!(initial_funding.len(), 1);
	assert_eq!(initial_funding.values().map(|output| output.value).sum::<Amount>(), sat(500_000));
	let first = wallet.board(sat(100_000)).await;
	let second = wallet.board(sat(100_000)).await;
	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	wallet.sync().await;
	assert_eq!((first.vtxos.len(), second.vtxos.len()), (1, 1));
	let client = wallet.client().await;
	let exiting = client.get_full_vtxo(first.vtxos[0]).await.unwrap();
	let untouched = client.get_full_vtxo(second.vtxos[0]).await.unwrap();
	let destination = client.fallback_script();
	let owner_mailbox = client.mailbox_identifier().as_pubkey();

	let exiting_key = client.get_vtxo_key(&exiting).await.unwrap();
	assert_ne!(exiting.chain_anchor(), untouched.chain_anchor());
	assert_eq!(exiting.expiry_height(), untouched.expiry_height());
	assert_eq!(exiting.amount() + untouched.amount(), sat(200_000));
	let root_exit_txid = exiting.transactions().find(|step|
		step.tx.input.iter().any(|input| input.previous_output == exiting.chain_anchor()))
		.expect("the holder must possess the signed path that actually spends this root").tx.compute_txid();
	drop(client);
	let mut backing = BTreeMap::new();
	let mut boards = BTreeMap::new();
	for coin in [&exiting, &untouched] {
		let funding = ctx.bitcoind().await_transaction(coin.chain_anchor().txid).await;
		coin.validate(&funding).unwrap();
		backing.insert(coin.chain_anchor(), funding.output[coin.chain_anchor().vout as usize].clone());
		boards.insert(funding.compute_txid(), funding);
	}
	assert_eq!(boards.len(), 2);
	let mut board_fee = Amount::ZERO;
	for board in boards.values() { board_fee += actual_fee(&ctx, board).await; }
	assert!(board_fee > Amount::ZERO);
	// Capture the original board change BEFORE any exit/CPFP/user-claim can
	// spend it on the later orphaned branch. The holder is never reopened after
	// dropping below; post-fork proof uses only exact Core UTXO observations.
	let mut ordinary_change = BTreeMap::new();
	for utxo in wallet.onchain_utxos().await {
		assert!(boards.contains_key(&utxo.outpoint.txid));
		assert!(!backing.contains_key(&utxo.outpoint));
		let transaction = ctx.bitcoind().await_transaction(utxo.outpoint.txid).await;
		let output = transaction.output[utxo.outpoint.vout as usize].clone();
		assert_eq!(output.value, utxo.amount);
		assert_ne!(output.script_pubkey, destination);
		assert!(ordinary_change.insert(utxo.outpoint, output).is_none());
	}
	assert!(!ordinary_change.is_empty());
	assert_eq!(sat(500_000), backing.values().map(|output| output.value).sum::<Amount>()
		+ ordinary_change.values().map(|output| output.value).sum::<Amount>() + board_fee);
	wallet.start_exit_vtxos([exiting.id()]).await;
	let height = ctx.bitcoind().get_block_count().await as u32;
	ctx.generate_blocks(exiting.expiry_height().to_u32() + 1 - height).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let coins = vec![exiting.id().to_string(), untouched.id().to_string()];
	// Each root is its own decision and its own payout transaction.
	let originals = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let rows = db.read(async |tx| Ok(tx.query("SELECT c.vtxo_id,a.id,a.sweep_tx \
				FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				WHERE c.vtxo_id=ANY($1) AND a.sweep_tx IS NOT NULL AND a.broadcast_at IS NOT NULL",
				&[&coins],
			).await?)).await.unwrap();
			if rows.len() == coins.len() {
				break rows.into_iter().map(|row| (row.get::<_, String>("vtxo_id"),
					(row.get::<_, i64>("id"), deserialize::<Transaction>(row.get("sweep_tx")).unwrap())))
					.collect::<BTreeMap<_, _>>();
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("each root's customer decision must be durably signed");
	for coin in [&exiting, &untouched] {
		let (_, original) = &originals[&coin.id().to_string()];
		assert_eq!(original.version.0, 2);
		assert_eq!(original.input.len(), 1);
		assert_eq!(original.input[0].previous_output, coin.chain_anchor());
		let original_fee = actual_fee(&ctx, original).await;
		assert!(original_fee > Amount::ZERO);
		assert_eq!(original.output.iter().map(|out| out.value.to_sat()).sum::<u64>() + original_fee.to_sat(),
			backing[&coin.chain_anchor()].value.to_sat(),
			"the direct payout must conserve actual backing and its inline miner fee");
		let payouts = original.output.iter().filter(|out| out.script_pubkey == destination).collect::<Vec<_>>();
		assert_eq!(payouts.len(), 1);
		assert_eq!(payouts[0].value, sat(98_000));
		assert!(ctx.bitcoind().sync_client().get_raw_mempool().unwrap().contains(&original.compute_txid()));
	}
	let (original_id, _) = originals[&coins[0]];
	let (untouched_id, _) = originals[&coins[1]];
	let permanent = decided_obligations(&db, &coins).await;

	// The direct payout is immediately relayable, unlike the old zero-fee
	// parent. Freeze both daemons and clear the local regtest mempool before
	// constructing the competing holder-exit branch.
	srv.watchmand().stop().await.unwrap();
	srv.stop().await.unwrap();
	let loaded_wallets = ctx.bitcoind().sync_client().list_wallets().unwrap();
	ctx.bitcoind().restart_wiping_mempool().await;
	for name in loaded_wallets {
		if !ctx.bitcoind().sync_client().list_wallets().unwrap().contains(&name) {
			ctx.bitcoind().load_wallet(&name).await;
		}
	}
	assert!(ctx.bitcoind().sync_client().get_raw_mempool().unwrap().is_empty());
	progress_exit_until_awaiting_delta(&ctx, &wallet).await;
	let conflict = ctx.bitcoind().sync_client().custom_get_raw_transaction_info(root_exit_txid, None)
		.unwrap().unwrap();
	let conflict_block = conflict.blockhash.expect("the backing conflict must really confirm");
	assert!(conflict.confirmations.unwrap_or(0) > 0);
	assert!(ctx.bitcoind().sync_client().get_tx_out(&exiting.chain_anchor().txid,
		exiting.chain_anchor().vout, Some(true)).unwrap().is_none());
	srv.start().await.unwrap();
	srv.watchmand().start().await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let status = db.read(async |tx| Ok(tx.query_one("SELECT status FROM expiry_allocation_state WHERE id=$1",
				&[&original_id]).await?.get::<_, String>(0))).await.unwrap();
			if status == "conflicted" { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the actual intermediate conflict must invalidate the exiting root's decision");
	srv.watchmand().stop().await.unwrap();
	complete_exit(&ctx, &wallet).await;
	let claim_address = ctx.bitcoind().get_new_address();
	wallet.claim_exits([exiting.id()], &claim_address).await;
	ctx.generate_blocks(1).await;
	assert!(ctx.bitcoind().get_received_by_address(&claim_address) > sat(0));
	// No more holder commands after the real claim. In particular it cannot
	// rebroadcast orphaned signed paths when the owned Core mempool is cleared.
	drop(wallet);
	srv.watchmand().start().await.unwrap();
	let claim_txid = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let txid = db.read(async |tx| Ok(tx.query_one("SELECT onchain_spent_txid FROM vtxo WHERE vtxo_id=$1",
				&[&coins[0]]).await?.get::<_, Option<String>>(0))).await.unwrap();
			if let Some(txid) = txid { break txid.parse::<Txid>().unwrap(); }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the watchman must see the holder's claim");
	let untouched_status = db.read(async |tx| Ok(tx.query_one("SELECT status FROM expiry_allocation_state WHERE id=$1",
		&[&untouched_id]).await?.get::<_, String>(0))).await.unwrap();
	assert_ne!(untouched_status, "conflicted", "the untouched root keeps its own decision");
	let claim = ctx.bitcoind().await_transaction(claim_txid).await;
	assert!(claim.input.iter().any(|input| input.previous_output == exiting.point()));
	// The holder put the coin on chain itself, so no payout is reported.
	let status = srv.get_public_rpc().await.get_vtxo_status(protos::GetVtxoStatusRequest {
		vtxo_id: exiting.id().to_bytes().to_vec(),
		attestation: VtxoStatusAttestation::new(exiting.id(), &exiting_key).serialize(),
	}).await.unwrap().into_inner();
	assert_eq!(status.spend_state, protos::VtxoSpendState::Spent as i32);
	assert!(status.fallback_payout.is_none(), "an exited coin is not a pending payout");
	assert!(claim.output.iter().any(|out| out.script_pubkey == claim_address.script_pubkey() && out.value > sat(0)));
	let old_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	rebuilt_generation(&db, old_tip).await;
	assert_eq!(decided_obligations(&db, &coins).await, permanent);

	// Remove the FIRST root-spending block, not just the terminal-claim block.
	// The full backing conflict and its descendant terminal claim disappear.
	srv.watchmand().stop().await.unwrap();
	// Upstream sync rejects a lower tip; restart only on the higher fork.
	srv.stop().await.unwrap();
	ctx.bitcoind().sync_client().invalidate_block(&conflict_block).unwrap();
	let fork = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	let removed = old_tip.height.to_u32() - fork.height.to_u32();
	assert!(removed > 0 && removed <= 32, "keep the real exit/claim branch bounded");
	// Existing isolated-node crash/restart helper; no database edits and no
	// third-party process is involved. Clearing orphaned mempool transactions
	// prevents the discarded holder branch from being mined again by accident.
	let loaded_wallets = ctx.bitcoind().sync_client().list_wallets().unwrap();
	ctx.bitcoind().restart_wiping_mempool().await;
	// TestContext loads its own mining wallet explicitly after snapshot start;
	// a Core restart is not guaranteed to load it again automatically.
	for name in loaded_wallets {
		if !ctx.bitcoind().sync_client().list_wallets().unwrap().contains(&name) {
			ctx.bitcoind().load_wallet(&name).await;
		}
	}
	let rpc = ctx.bitcoind().sync_client();
	assert!(rpc.get_raw_mempool().unwrap().is_empty());
	for coin in [&exiting, &untouched] {
		let root = rpc.get_tx_out(&coin.chain_anchor().txid, coin.chain_anchor().vout, Some(true))
			.unwrap().expect("the replacement branch must restore both genuine backing roots");
		assert!(root.confirmations > 0);
		assert_eq!(root.value, backing[&coin.chain_anchor()].value);
		assert_eq!(root.script_pub_key.script().unwrap(), backing[&coin.chain_anchor()].script_pubkey);
	}
	for (point, expected) in &ordinary_change {
		let actual = rpc.get_tx_out(&point.txid, point.vout, Some(true)).unwrap()
			.expect("the complete conflicting branch rollback must restore original ordinary board change");
		assert!(actual.confirmations > 0);
		assert_eq!(actual.value, expected.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), expected.script_pubkey);
	}
	let mining_address = ctx.bitcoind().get_new_address();
	rpc.generate_to_address(u64::from(removed + 2), &mining_address).unwrap();
	// Two separate canonical winners are valid. Provide two independent
	// confirmed sponsors so an increased fee target can use them if needed.
	let mut fee_sponsors = BTreeMap::new();
	for _ in 0..2 {
		let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
		let funding = ctx.bitcoind().await_transaction(txid).await;
		let outputs = funding.output.iter().enumerate().filter(|(_, output)|
			output.script_pubkey == fee_address.script_pubkey() && output.value == sat(100_000)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		let (vout, output) = outputs[0];
		assert!(fee_sponsors.insert(OutPoint::new(txid, vout as u32), output.clone()).is_none());
	}
	assert_eq!(fee_sponsors.len(), 2);
	// Current native sync helpers skip stopped daemons. Direct generation here
	// makes the intended same-Core/offline-product interval explicit; it is not
	// a fix for a proven stopped-daemon synchronization deadlock.
	ctx.bitcoind().generate(2).await;
	for (point, expected) in &fee_sponsors {
		let actual = rpc.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 2);
		assert_eq!(actual.value, expected.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), expected.script_pubkey);
	}
	let replacement_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	assert!(replacement_tip.height > old_tip.height);
	srv.start().await.unwrap();
	srv.watchmand().start().await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let invalid = db.read(async |tx| tx.get_highest_block(BlockTable::Captaind).await).await.unwrap() == Some(replacement_tip);
			if invalid { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the actual conflict rollback must reach the higher canonical replacement tip");
	assert_eq!(decided_obligations(&db, &coins).await, permanent);
	rebuilt_generation(&db, replacement_tip).await;
	assert_eq!(decided_obligations(&db, &coins).await, permanent);
	let spender = db.read(async |tx| Ok(tx.query_one("SELECT onchain_spent_txid FROM vtxo WHERE vtxo_id=$1",
		&[&coins[0]]).await?.get::<_, Option<String>>(0))).await.unwrap();
	assert_ne!(spender, Some(claim_txid.to_string()), "an orphaned claim must not remain the coin's spend");
	assert!(rpc.custom_get_raw_transaction_info(claim_txid, None).unwrap()
		.is_none_or(|info| info.confirmations.unwrap_or(0) == 0));
	// The exiting root's payout is valid again once its root is restored.
	// Observe real transactions covering both restored roots; the canonical
	// winner, not allocation age, determines payment. Preserve every signed
	// parent as it is observed.
	let mut stored_parents = originals.values().cloned().collect::<BTreeMap<_, _>>();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let rows = db.read(async |tx| Ok(tx.query("SELECT DISTINCT a.id,a.sweep_tx \
				FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				WHERE c.vtxo_id=ANY($1) AND a.sweep_tx IS NOT NULL", &[&coins],
			).await?)).await.unwrap();
			let mut covered = BTreeSet::new();
			let mempool = rpc.get_raw_mempool().unwrap();
			for row in rows {
				let id = row.get::<_, i64>("id");
				let parent = deserialize::<Transaction>(row.get("sweep_tx")).unwrap();
				if let Some(saved) = stored_parents.get(&id) { assert_eq!(saved, &parent); }
				else { stored_parents.insert(id, parent.clone()); }
				if mempool.contains(&parent.compute_txid()) {
					covered.extend(parent.input.iter().map(|input| input.previous_output));
				}
			}
			if backing.keys().all(|root| covered.contains(root)) { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("real fallback packages must cover both restored roots without the old user claim");
	ctx.generate_blocks(6).await;
	let paid_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	rebuilt_generation(&db, paid_tip).await;
	let mut canonical = BTreeMap::new();
	let mut root_payments = BTreeMap::new();
	for parent in stored_parents.values() {
		let txid = parent.compute_txid();
		let Some(info) = rpc.custom_get_raw_transaction_info(txid, None).unwrap() else { continue; };
		if info.confirmations.unwrap_or(0) == 0 { continue; }
		assert!(canonical.insert(txid, info.blockhash.unwrap()).is_none());
		for input in &parent.input {
			assert!(backing.contains_key(&input.previous_output), "this payout must spend only actual restored roots");
			assert!(root_payments.insert(input.previous_output, txid).is_none(),
				"one real backing root cannot have two canonical economic payments");
		}
	}
	assert_eq!(root_payments.keys().copied().collect::<BTreeSet<_>>(), backing.keys().copied().collect::<BTreeSet<_>>());
	assert_eq!(canonical.len(), 2, "each root is paid exactly once by its own payout");
	let expected_payers = BTreeMap::from([
		(coins[0].clone(), root_payments[&exiting.chain_anchor()]),
		(coins[1].clone(), root_payments[&untouched.chain_anchor()]),
	]);
	assert_final_outcome(&ctx, &db, &coins, &expected_payers, &stored_parents, &canonical).await;
	let physical_cut = Box::pin(assert_physical_cut(&ctx, &db, &srv, &coins, &expected_payers,
		&stored_parents, &canonical, &backing, &fee_sponsors, &destination,
		&owner_mailbox.to_string(), &initial_funding, &boards, &ordinary_change, board_fee)).await;
	assert_eq!(decided_obligations(&db, &coins).await, permanent);
	// Reconsidering a shorter real branch must not resurrect the old claim's
	// economic payment or supersede the longer canonical payout branch.
	rpc.reconsider_block(&conflict_block).unwrap();
	assert_eq!(server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap(), paid_tip);
	// Give both listeners a real post-reconsider cycle before checking the
	// outcome. An immediate Core/fence read could miss stale asynchronous
	// terminal-proof or active-payer reconstruction.
	ctx.generate_blocks(1).await;
	let reconsidered_tip = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
	rebuilt_generation(&db, reconsidered_tip).await;
	assert_final_outcome(&ctx, &db, &coins, &expected_payers, &stored_parents, &canonical).await;
	assert_eq!(Box::pin(assert_physical_cut(&ctx, &db, &srv, &coins, &expected_payers,
		&stored_parents, &canonical, &backing, &fee_sponsors, &destination,
		&owner_mailbox.to_string(), &initial_funding, &boards, &ordinary_change, board_fee)).await, physical_cut,
		"a settled reconsider cycle must retain the same canonical signed packages and physical money cut");
	assert!(rpc.custom_get_raw_transaction_info(claim_txid, None).unwrap()
		.is_none_or(|info| info.confirmations.unwrap_or(0) == 0));
	assert!(!rpc.get_raw_mempool().unwrap().contains(&claim_txid));
	assert_eq!(decided_obligations(&db, &coins).await, permanent);
}
