use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::PaymentHash;
use ark::{VtxoId, VtxoPolicy};
use ark_testing::{sat, TestContext};
use ark_testing::constants::BOARD_CONFIRMATIONS;
use bitcoin::{OutPoint, Transaction, TxOut};
use bitcoin::consensus::deserialize;
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::{Db, SpendState};
use server_rpc::protos;

async fn assert_pending_root_is_untouched(
	ctx: &TestContext, db: &Db, htlc: VtxoId, root: OutPoint, backing: &TxOut,
) {
	let row = db.read(async |tx| Ok(tx.query_one("SELECT \
		(SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1) AS entitlements, \
		(SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1) AS fences, \
		(SELECT COUNT(*) FROM expiry_allocation_input WHERE vtxo_id=$2) AS allocations, \
		(SELECT to_regclass('expiry_split_request') IS NULL) AS no_split_table, \
		(SELECT COUNT(*) FROM lightning_payment_attempt_htlc_vtxo WHERE vtxo_id=$1) AS attempts",
		&[&htlc.to_string(), &VtxoId::from(root).to_string()],
	).await?)).await.unwrap();
	assert!(row.get::<_, bool>("no_split_table"));
	for field in ["entitlements", "fences", "allocations", "attempts"] {
		assert_eq!(row.get::<_, i64>(field), 0,
			"the unresolved tree must remain pending before its HTLC deadline: {field}");
	}
	let state = db.read(async |tx|
		server::database::htlc_vtxo::get_htlc_vtxo(tx, htlc).await
	).await.unwrap().expect("the genuine registered HTLC must remain present");
	assert_eq!(state.htlc.resolution(), None, "tree expiry is not HTLC settlement");
	let output = ctx.bitcoind().sync_client().get_tx_out(&root.txid, root.vout, Some(true))
		.unwrap().expect("the unresolved tree's actual backing must remain unspent");
	assert!(output.confirmations > 0);
	assert_eq!(output.value, backing.value);
	assert_eq!(output.script_pub_key.script().unwrap(), backing.script_pubkey);
}

/// Both real roots expire together. An unattempted signed HTLC under one
/// root cannot delay the other root's automatic, confirmed customer payout.
/// No financial rows, split requests or chain acknowledgments are fabricated.
#[tokio::test]
async fn expiry_ready_root_does_not_wait_for_unresolved_same_expiry_root() {
	let ctx = TestContext::new("server/expiry_ready_root_with_pending_same_expiry_root").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); }).watchmand().create().await;
	let wallet = ctx.bark("abandoned", &srv).funded(sat(500_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	// As in the native conflict test, create distinct roots before another
	// block so both have exactly the same expiry height.
	let ready_board = wallet.board(sat(100_000)).await;
	let pending_board = wallet.board(sat(100_000)).await;
	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	wallet.sync().await;
	assert_eq!((ready_board.vtxos.len(), pending_board.vtxos.len()), (1, 1));
	let client = wallet.client().await;
	let ready = client.get_full_vtxo(ready_board.vtxos[0]).await.unwrap();
	let pending = client.get_full_vtxo(pending_board.vtxos[0]).await.unwrap();
	assert_ne!(ready.chain_anchor(), pending.chain_anchor());
	assert_eq!(ready.expiry_height(), pending.expiry_height());
	let destination = client.fallback_script();
	let key = client.get_vtxo_key(&pending).await.unwrap();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	assert!(deadline > ready.expiry_height() + BlockDelta::new(7));
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([pending.clone()],
		VtxoPolicy::new_server_htlc_send(key.public_key(), PaymentHash::from([83; 32]), deadline))
		.unwrap().generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
	}).await.unwrap().into_inner();
	let grants = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(grants.len(), 1);
	assert_eq!(grants[0].amount(), pending.amount());
	assert_eq!(grants[0].chain_anchor(), pending.chain_anchor());
	client.register_vtxo_transactions_with_server(&grants).await.unwrap();
	let htlc = grants[0].id();
	// No external attempt is initiated, and no client is reopened or synced
	// after this point. Observation uses only Core and the server database.
	drop(client);
	drop(wallet);
	let funding = ctx.bitcoind().await_transaction(pending.chain_anchor().txid).await;
	let pending_backing = funding.output[pending.chain_anchor().vout as usize].clone();
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let height = ctx.bitcoind().get_block_count().await as u32;
	assert!(height < ready.expiry_height().to_u32());
	let expired = ctx.generate_blocks(ready.expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	assert!(expired < deadline);

	let (allocation, parent) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			assert_pending_root_is_untouched(&ctx, &db, htlc, pending.chain_anchor(), &pending_backing).await;
			let row = db.read(async |tx| Ok(tx.query_opt("SELECT a.id,a.sweep_tx \
				FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='customer' \
				AND a.broadcast_at IS NOT NULL ORDER BY a.id DESC LIMIT 1", &[&ready.id().to_string()],
			).await?)).await.unwrap();
			if let Some(row) = row {
				let parent = deserialize::<Transaction>(row.get("sweep_tx")).unwrap();
				break (row.get::<_, i64>("id"), parent);
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("a ready root must pay without waiting for the other root's HTLC deadline");
	assert_eq!(parent.input.len(), 1);
	assert_eq!(parent.input[0].previous_output, ready.chain_anchor());
	assert_eq!(parent.version.0, 2);
	assert!(parent.vsize() <= 100_000);
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	let payouts = parent.output.iter().enumerate()
		.filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(payouts.len(), 1);
	let (vout, payout) = payouts[0];
	assert_eq!(payout.value, ready.amount() - sat(2_000));
	let source = ctx.bitcoind().await_transaction(ready.chain_anchor().txid).await;
	assert!(source.output[ready.chain_anchor().vout as usize].value.to_sat() >=
		parent.output.iter().map(|output| output.value.to_sat()).sum::<u64>(),
		"the direct payout must not create value");
	let actual_fee = super::expiry_test::actual_fee(&ctx, &parent).await.to_sat();
	assert!(actual_fee > 0);
	let recorded_fee = db.read(async |tx| Ok(tx.query_one(
		"SELECT fee_sat FROM expiry_payout_attempt WHERE allocation_id=$1 AND txid=$2",
		&[&allocation, &parent.compute_txid().to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(actual_fee, u64::try_from(recorded_fee).unwrap());
	assert_pending_root_is_untouched(&ctx, &db, htlc, pending.chain_anchor(), &pending_backing).await;
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	assert!(confirmed < deadline, "the ready payout must confirm while the other HTLC is unresolved");
	let output = ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), vout as u32, Some(true))
		.unwrap().expect("the actual ready payout must be unspent in Core");
	assert!(output.confirmations >= 6);
	assert_eq!(output.value, payout.value);
	assert_eq!(output.script_pub_key.script().unwrap(), destination);
	assert_pending_root_is_untouched(&ctx, &db, htlc, pending.chain_anchor(), &pending_backing).await;
	let info = ctx.bitcoind().sync_client()
		.custom_get_raw_transaction_info(parent.compute_txid(), None).unwrap().unwrap();
	assert!(info.confirmations.unwrap_or(0) >= 6);
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |tx| Ok(tx.query_one("SELECT a.status,a.sweep_tx, \
				(SELECT COUNT(*) FROM expiry_allocation_coin c JOIN expiry_entitlement e \
				 ON e.id=c.entitlement_id WHERE e.vtxo_id=$2 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted')) AS active \
				FROM expiry_allocation_state a WHERE a.id=$1", &[&allocation, &ready.id().to_string()],
			).await?)).await.unwrap();
			assert_eq!(row.get::<_, i64>("active"), 1, "one ready entitlement has one active allocation");
			assert_eq!(deserialize::<Transaction>(row.get("sweep_tx")).unwrap(), parent);
			if row.get::<_, String>("status") == "confirmed" { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the ready payout must also reconcile as confirmed in the durable ledger");
	assert_eq!(db.read(async |tx| Ok(tx.get_user_vtxo_by_id(ready.id()).await?.spend_state))
		.await.unwrap(), SpendState::Spent);
}
