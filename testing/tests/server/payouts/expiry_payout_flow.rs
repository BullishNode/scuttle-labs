use std::time::Duration;

use ark::fees::{BoardFees, PpmFeeRate};
use ark_testing::{sat, TestContext};
use ark_testing::constants::BOARD_CONFIRMATIONS;
use ark_testing::payout_wallet::PayoutWallet;
use bitcoin::{OutPoint, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::RpcApi;
use server::database::Db;

/// No allocation, spend fence or chain acknowledgment is inserted by the test.
/// The running captaind and watchmand must discover, decide, sign and publish
/// the payout, and Bitcoin Core independently checks its real inputs and outputs.
#[tokio::test]
async fn expiry_payout_native_board_fee_seed_restore_and_confirmed_spend() {
	let ctx = TestContext::new("server/expiry_payout_native_board_fee_seed_restore").await;
	let srv = ctx.captaind("server").no_vtxo_pool()
		.cfg(move |cfg| { cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(24);
			cfg.fees.board = BoardFees { min_fee: sat(330), base_fee: sat(0), ppm: PpmFeeRate::ZERO };
		})
		.watchmand_cfg(|cfg| {
			cfg.fee_estimator.fallback_fee_rate_regular = bitcoin::FeeRate::from_sat_per_vb(30).unwrap();
		}).create().await;
	let wallet = ctx.bark("abandoned", &srv).create().await;
	ctx.fund_bark(&wallet, sat(200_000)).await;
	wallet.onchain_balance().await;
	let issuer = wallet.client().await;
	for _ in 0..250 { issuer.new_address_with_index().await.unwrap(); }
	drop(issuer);
	let quote = wallet.client().await.estimate_board_offchain_fee(sat(100_000)).await.unwrap();
	assert_eq!(quote.fee, sat(330));
	assert_eq!(quote.gross_amount, sat(100_000));
	assert_eq!(quote.net_amount + quote.fee, sat(100_000));
	assert!(quote.net_amount > sat(1_000));
	assert!(quote.vtxos_spent.is_empty());
	let board = wallet.board(sat(100_000)).await;
	assert_eq!(board.amount, quote.net_amount);

	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	wallet.sync().await;
	let ids = board.vtxos;
	assert_eq!(ids.len(), 1);
	let seed = wallet.mnemonic().await.unwrap();
	let client = wallet.client().await;
	let source = client.get_full_vtxo(ids[0]).await.unwrap();
	assert_eq!(source.amount(), quote.net_amount);
	let path = source.transactions().collect::<Vec<_>>();
	assert_eq!(path.len(), 1, "the board must have one signed exit");
	assert_eq!(path[0].tx.output.len(), 2);
	assert_eq!(path[0].tx.output[0].value, source.amount());
	assert_eq!(path[0].tx.output[1], bitcoin_ext::fee::fee_anchor_with_amount(quote.fee));
	let funding = ctx.bitcoind().await_transaction(source.chain_anchor().txid).await;
	assert_eq!(funding.output[source.chain_anchor().vout as usize].value, quote.gross_amount);
	assert_eq!(quote.gross_amount, source.amount() + quote.fee);

	let destination = client.fallback_script();
	let expected = source.amount() - sat(2_000);
	source.validate(&funding).unwrap();
	drop(client);

	// A confirmed watchman input is available if the operator remainder
	// cannot cover the fee inside the direct payout transaction.
	let fee_address = srv.watchmand().wait_wallet_address().await;
	let _fee_funding = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let height = ctx.bitcoind().get_block_count().await as u32;
	assert!(height < source.expiry_height().to_u32());
	let expired = ctx.generate_blocks(source.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;

	// Do not open or sync the abandoned wallet while waiting. In particular,
	// its expired input must not be refreshed by a test helper before deciding.
	let (allocation, parent) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {

			let no_split_table = db.read(async |t| Ok(t.query_one(
				"SELECT to_regclass('expiry_split_request') IS NULL", &[],
			).await?.get::<_, bool>(0))).await.unwrap();
			assert!(no_split_table, "unresolved ownership must not use an expiry split path");
			if let Some((id, parent)) = super::expiry_test::published_payout(&db, source.id()).await {
				break (id, parent);
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("real background payout did not reach publication; inspect both daemon logs");
	let parent_txid = parent.compute_txid();
	assert_eq!(ctx.bitcoind().await_transaction(parent_txid).await, parent);
	assert_eq!(parent.version.0, 2);
	assert!(parent.vsize() <= 100_000);
	let payouts = parent.output.iter().enumerate()
		.filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(payouts.len(), 1, "one user in one backing output pays one fee");
	assert_eq!(payouts[0].1.value, expected);
	let payout_point = OutPoint::new(parent_txid, payouts[0].0 as u32);
	let actual_fee = super::expiry_test::actual_fee(&ctx, &parent).await;
	assert!(actual_fee > bitcoin::Amount::ZERO);
	assert!(parent.input.iter().any(|input| input.previous_output == source.chain_anchor()));
	assert!(!parent.output.iter().any(|output| output.script_pubkey == *bitcoin_ext::fee::P2A_SCRIPT));
	let operator_script = srv.watchmand().config().sweep_address.clone().unwrap()
		.require_network(bitcoin::Network::Regtest).unwrap().script_pubkey();
	assert!(parent.output.iter().filter(|output| output.script_pubkey != destination)
		.all(|output| output.script_pubkey == operator_script));
	assert!(parent.output.iter().filter(|output| output.script_pubkey == operator_script).count() <= 1);
	let operator_output = parent.output.iter().filter(|output| output.script_pubkey != destination)
		.map(|output| output.value).sum::<bitcoin::Amount>();
	let mut sponsor_sat = bitcoin::Amount::ZERO;
	for input in parent.input.iter().filter(|input| input.previous_output != source.chain_anchor()) {
		let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
		sponsor_sat += previous.output[input.previous_output.vout as usize].value;
	}
	assert!(sponsor_sat > bitcoin::Amount::ZERO, "high fee target must exercise a watchman input");
	assert_eq!(expected + operator_output + actual_fee, quote.gross_amount + sponsor_sat);
	assert_eq!(operator_output + actual_fee, quote.fee + sat(2_000) + sponsor_sat,
		"the operator output contains only the board fee, payout charge and sponsor principal after mining");
	ctx.generate_blocks(6).await;
	let confirmed_payout = ctx.bitcoind().sync_client().get_tx_out(&parent_txid, payout_point.vout, Some(true))
		.unwrap().expect("fallback output must be independently unspent in Core");
	assert!(confirmed_payout.confirmations >= 6, "a mempool parent is not a confirmed payout");
	assert_eq!(confirmed_payout.value, expected);

	// Recover only from the test seed, in a Bitcoin wallet created after the
	// payout confirmed. Bark's Ark balance and onchain wallet are not involved.
	let payout_dir = ctx.datadir.join("payout-wallet");
	let mut restored = PayoutWallet::restore(&seed.to_seed(""), bitcoin::Network::Regtest, &payout_dir).unwrap();
	assert_eq!(restored.script(), &destination);
	restored.rescan(ctx.bitcoind()).unwrap();
	restored.confirmed_payout(payout_point, expected).unwrap();
	let recovery_address = ctx.bitcoind().get_new_address();
	let spend = restored.spend(ctx.bitcoind(), payout_point, expected, &recovery_address,
		bitcoin::FeeRate::from_sat_per_vb(2).unwrap(), &payout_dir).unwrap();
	assert!(spend.fee > bitcoin::Amount::ZERO);
	let recovery = ctx.bitcoind().await_transaction(spend.txid).await;
	assert!(recovery.input.iter().any(|input| input.previous_output == payout_point),
		"the independent test wallet must spend this actual fallback output");
	ctx.generate_blocks(1).await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&spend.output.txid, spend.output.vout, Some(true))
		.unwrap().unwrap().confirmations >= 1);
	assert!(ctx.bitcoind().get_received_by_address(&recovery_address) > sat(0));
	assert!(ctx.bitcoind().sync_client().get_tx_out(&parent_txid, payout_point.vout, Some(true)).unwrap().is_none());
	assert_eq!(ctx.bitcoind().await_transaction(recovery.compute_txid()).await, recovery);
	let raw = db.read(async |t| Ok(t.query_one("SELECT sweep_tx FROM expiry_allocation_state WHERE id=$1", &[&allocation]).await?
		.get::<_, Vec<u8>>(0))).await.unwrap();
	assert_eq!(deserialize::<Transaction>(&raw).unwrap(), parent, "confirmation/recovery must not rewrite the payout parent");
	// Preserve the one-block spend check, then require six confirmations.
	ctx.generate_blocks(5).await;
	let deep = ctx.bitcoind().sync_client().get_tx_out(&spend.output.txid, spend.output.vout, Some(true))
		.unwrap().unwrap();
	assert!(deep.confirmations >= 6);
}

/// Watchman inputs may pay the mining fee of a payout, never an owner. An
/// allocation whose owner outputs exceed its root is refused at the store,
/// even with enough watchman funds to build it.
#[tokio::test]
async fn expiry_payout_never_pays_owners_more_than_the_root() {
	let ctx = TestContext::new("server/expiry_payout_never_pays_owners_more_than_the_root").await;
	// At this rate the payout charge cannot cover the mining fee, so no
	// payout is built until the watchman is funded.
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(|cfg| {
			cfg.fee_estimator.fallback_fee_rate_regular = bitcoin::FeeRate::from_sat_per_vb(30).unwrap();
		}).create().await;
	let wallet = ctx.bark("owner", &srv).funded(sat(500_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let board = wallet.board(sat(100_000)).await;
	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	wallet.sync().await;
	let client = wallet.client().await;
	let coin = client.get_full_vtxo(board.vtxos[0]).await.unwrap();
	let destination = client.fallback_script();
	drop(client);
	drop(wallet);
	let funding = ctx.bitcoind().await_transaction(coin.chain_anchor().txid).await;
	let root = funding.output[coin.chain_anchor().vout as usize].value;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;

	let id = coin.id().to_string();
	let (allocation, gross, fee) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if let Some(row) = db.read(async |t| Ok(t.query_opt("SELECT c.allocation_id,c.gross_sat,c.fee_sat \
				FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND c.disposition='customer'", &[&id]).await?)).await.unwrap() {
				break (row.get::<_, i64>(0), row.get::<_, i64>(1), row.get::<_, i64>(2));
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the expired board must be decided");
	let attempts = async || db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM expiry_payout_attempt \
		WHERE allocation_id=$1", &[&allocation]).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(attempts().await, 0, "an unfunded payout is not stored");

	// Make the stored owner amount exceed the root, then fund the watchman.
	let excess = i64::try_from(root.to_sat()).unwrap() + 1_000;
	let set_payout = async |payout: i64| db.write(async |t| Ok(t.execute("UPDATE expiry_allocation_coin \
		SET payout_sat=$2,subsidy_sat=$2+fee_sat-gross_sat WHERE allocation_id=$1 AND disposition='customer'",
		&[&allocation, &payout]).await?)).await.unwrap();
	assert_eq!(set_payout(excess).await, 1);
	ctx.bitcoind().fund_addr(srv.watchmand().wait_wallet_address().await, sat(1_000_000)).await;
	for _ in 0..4 {
		let tip = ctx.generate_blocks(1).await;
		srv.watchmand().wait_for_sync_height(tip).await;
		srv.watchmand().trigger_sweep().await;
		tokio::time::sleep(Duration::from_millis(500)).await;
		assert_eq!(attempts().await, 0, "a payout above its root must never be stored");
	}

	// With the decided amount restored, the same funds pay the owner.
	assert_eq!(set_payout(gross - fee).await, 1);
	srv.watchmand().trigger_sweep().await;
	let (_, parent) = super::expiry_test::wait_payout(&db, coin.id()).await;
	let paid = parent.output.iter().filter(|output| output.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(paid.len(), 1);
	assert_eq!(paid[0].value, coin.amount() - sat(2_000));
	assert!(paid[0].value <= root);
}
