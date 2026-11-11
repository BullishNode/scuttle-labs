pub(super) use super::expiry_test::{confirmed_grant_closures, assert_grants_reclaimed};

use std::time::Duration;

use ark::lightning::{PaymentHash, Preimage};
use ark::{ProtocolEncoding, Vtxo, VtxoId, VtxoPolicy};
use ark::vtxo::Full;
use ark_testing::{btc, sat, TestContext};
use ark_testing::context::LightningPaymentSetup;
use bitcoin::Transaction;
use bitcoin_ext::{AmountExt, BlockDelta, BlockHeight};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use cln_rpc::plugins::hold;
use server::database::Db;
use server_log::{ExpiryHoldObserved, XpayRpcCalled, XpayRpcReturned};
use server_rpc::protos;

/// Grant delivery must leave every signed ancestor durable on the
/// server, even when neither wallet ever returns to register or claim it.
pub(super) async fn assert_stored_grant_ancestry(db: &Db, grants: &[Vtxo<Full>]) {
	let mut expected = std::collections::BTreeMap::new();
	for grant in grants {
		for step in grant.transactions() {
			let raw = bitcoin::consensus::serialize(&step.tx);
			if let Some(old) = expected.insert(step.tx.compute_txid().to_string(), raw.clone()) {
				assert_eq!(old, raw, "shared signed grant ancestry must be exact");
			}
		}
	}
	let ids = expected.keys().cloned().collect::<Vec<_>>();
	let actual = db.read(async |t| Ok(t.query("SELECT txid,signed_tx FROM virtual_transaction WHERE txid=ANY($1)",
		&[&ids]).await?)).await.unwrap().into_iter().map(|row|
		(row.get::<_, String>("txid"), row.get::<_, Option<Vec<u8>>>("signed_tx")))
		.collect::<std::collections::BTreeMap<_,_>>();
	assert_eq!(actual.len(), expected.len(), "server is missing a granted receive's ancestor");
	for (txid, raw) in expected {
		assert_eq!(actual.get(&txid).and_then(Option::as_ref), Some(&raw),
			"server lost the actual signature/preimage for receive ancestor {txid}");
	}
}

async fn failed_external_payout(settle_during_decision: bool) {
	let name = if settle_during_decision { "server/expiry_settlement_during_decision_keeps_source_held" }
		else { "server/maxdelay_uses_server_tip_when_cln_lags" };
	let ctx = TestContext::new(name).await;
	let lightning = {
		let internal = ctx.lightningd("lightningd_internal").own_bitcoind().create().await;
		let external = ctx.lightningd("lightningd_external").create().await;
		ctx.fund_lightning(&internal, btc(12)).await;
		let height = ctx.generate_blocks(6).await;
		internal.wait_for_block(height).await;
		LightningPaymentSetup { internal, external }
	};
	let srv = ctx.captaind("server").lightningd(&lightning.internal).no_vtxo_pool()
		.funded(btc(10)).cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(400); }).watchmand().create().await;
	let wallet = ctx.bark("abandoned", &srv).funded(sat(200_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = wallet.client().await;
	let input = client.get_full_vtxo(ids[0]).await.unwrap();
	let source_expiry = input.expiry_height();
	let key = client.get_vtxo_key(&input).await.unwrap();
	let destination = client.fallback_script();
	let preimage = Preimage::random();
	let hash = preimage.compute_payment_hash();
	let mut receiver = lightning.external.hold_client().await;
	let invoice = receiver.invoice(hold::InvoiceRequest {
		payment_hash: hash.to_vec(), amount_msat: 90_000_000,
		description: Some(hold::invoice_request::Description::Memo("failed external offline owner".into())),
		min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
	}).await.unwrap().into_inner().bolt11;
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let source = super::expiry_send_claim::signed_leaf(&ctx, &srv, &client, input,
		VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).await;
	let id = source.id();
	lightning.sync().await;
	let (native_tip, server_tip) = {
		// This is a genuine older native chain view, not a fake getinfo
		// response. RPC stays available on the existing authenticated node.
		assert!(!lightning.internal.bitcoind().sync_client()
			.call::<bool>("setnetworkactive", &[serde_json::json!(false)]).unwrap());
		let native_tip = lightning.internal.grpc_client().await.getinfo(cln_rpc::GetinfoRequest {}).await.unwrap().into_inner().blockheight;
		// Use the primary directly: the normal all-node barrier must not wait
		// for the deliberately disconnected secondary until it is restored.
		ctx.bitcoind().generate(1).await;
		let server_tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
		srv.wait_for_sync_height(server_tip).await;
		assert!(server_tip.to_u32() > native_tip);
		assert_eq!(lightning.internal.grpc_client().await.getinfo(cln_rpc::GetinfoRequest {}).await.unwrap().into_inner().blockheight, native_tip);
		(native_tip, server_tip)
	};
	let dispatched_invoice = invoice.clone();
	let mut rpc = srv.get_public_rpc().await;
	let mut xpay_called = srv.subscribe_log::<XpayRpcCalled>();
	let mut xpay_returned = srv.subscribe_log::<XpayRpcReturned>();
	rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
		protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: 90_000, htlc_vtxo_ids: vec![id.to_bytes().to_vec()],
		mailbox_id: None, retry_for_secs: Some(5),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	}, &key).await).await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (attempt, native_call) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_opt("SELECT a.id FROM lightning_payment_attempt a \
				JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id \
				WHERE a.payment_hash=$1 AND l.vtxo_id=$2 AND a.status='failed'",
				&[&hash.to_string(), &id.to_string()]).await?)).await.unwrap();
			if let Some(row) = row {
				let call = (xpay_called.recv().await.expect("xpay call log missing"),
					xpay_returned.recv().await.expect("xpay reply log missing"));
				break (row.get::<_, i64>("id"), call);
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual CLN dispatch never reached upstream Failed");
	let native = lightning.internal.grpc_client().await.list_send_pays(cln_rpc::ListsendpaysRequest {
		payment_hash: Some(hash.to_vec()), ..Default::default()
	}).await.unwrap().into_inner().payments;
	assert!(native.iter().all(|payment| payment.status == cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Failed as i32));
	assert!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	// This native completion is the missing zero-part authority. Neither
	// empty parts nor the copied failed status is its substitute.
	assert!(native.is_empty(), "the no-channel case must exercise a genuine zero-part reply");
	let label = ark_testing::lightning::attempt_xpay_label(&db, attempt).await;
	let row = db.read(async |t| Ok(t.query_one("SELECT x.id,x.lightning_node_id,x.label,x.payment_hash, \
		x.payment_amount_msat,x.max_routing_fee_msat,x.status::TEXT,x.error_code,a.error, \
		x.signed_invoice,x.htlc_expiry,a.retry_for_secs, \
		a.lightning_node_id AS attempt_node,n.pubkey FROM cln_xpay x \
		JOIN lightning_payment_attempt a ON a.id=$1 JOIN lightning_node n ON n.id=x.lightning_node_id \
		WHERE x.label=$2", &[&attempt, &label]).await?)).await.unwrap();
	assert_eq!(row.get::<_, &str>("status"), "fail");
	assert_eq!(row.get::<_, Option<i32>>("error_code"), None);
	assert_eq!(row.get::<_, &str>("label"), label);
	assert_eq!(row.get::<_, &str>("payment_hash"), hash.to_string());
	assert_eq!(row.get::<_, i64>("payment_amount_msat"), 90_000_000);
	assert_eq!(row.get::<_, i64>("lightning_node_id"), row.get::<_, i64>("attempt_node"));
	let (called, returned) = native_call;
	assert_eq!(called.payment_hash, hash);
	assert_eq!(returned.payment_hash, hash);
	assert_eq!(called.invoice, dispatched_invoice);
	assert_eq!(called.payment_amount, sat(90_000));
	assert_eq!(called.max_routing_fee.to_msat(),
		u64::try_from(row.get::<_, Option<i64>>("max_routing_fee_msat").unwrap()).unwrap());
	assert_eq!(row.get::<_, Option<&str>>("signed_invoice"), Some(dispatched_invoice.as_str()));
	assert_eq!(row.get::<_, Option<i32>>("retry_for_secs"), Some(5));
	let native_error = returned.error.as_deref().expect("NoRoute must return a native error");
	assert_eq!(row.get::<_, &str>("error"), native_error);
	assert!(native_error.contains("Error calling method Xpay: RpcError { code: Some(205), message: "));
	let info = lightning.internal.grpc_client().await.getinfo(cln_rpc::GetinfoRequest {}).await.unwrap().into_inner();
	assert!(!info.version.is_empty());
	assert_eq!(info.network, srv.config().network.to_string());
	assert_eq!(row.get::<_, Vec<u8>>("pubkey"), info.id);
	assert_eq!(info.blockheight, native_tip);
	let maxdelay = u64::from(called.max_delay);
	let recorded_deadline = u64::try_from(row.get::<_, Option<i32>>("htlc_expiry").unwrap()).unwrap();
	assert!(u64::from(server_tip.to_u32()) + maxdelay <= recorded_deadline);
	assert!(u64::from(native_tip) + maxdelay < recorded_deadline,
		"a genuinely lagging native tip must not enlarge the relative delay");

	assert!(lightning.internal.bitcoind().sync_client()
		.call::<bool>("setnetworkactive", &[serde_json::json!(true)]).unwrap());
	ctx.await_block_count_sync().await;
	lightning.internal.wait_for_block_sync().await;

	drop(client);
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	if settle_during_decision {
		let mut holds = srv.watchmand().subscribe_log::<ExpiryHoldObserved>();
		let settler = db.clone();
		db.write(async |tx| {
			// The decision analyses the failed send as the sender's refund, then
			// waits here; a settlement for its hash commits before the write.
			tx.execute("SELECT pg_advisory_xact_lock($1,$2)",
				&[&5_i32, &i32::try_from(source_expiry.to_u32())?]).await?;
			let height = ctx.bitcoind().get_block_count().await as u32;
			ctx.generate_blocks(source_expiry.to_u32() + 1 - height).await;
			super::expiry_operation_races::wait_for_query_lock(tx, "SELECT pg_advisory_xact_lock($1,$2)").await?;
			settler.write(async |t| t.store_htlc_settlement(preimage).await).await?;
			Ok(())
		}).await.unwrap();
		ctx.generate_blocks(1).await;
		tokio::time::timeout(Duration::from_secs(30), async {
			while let Some(hold) = holds.recv().await {
				if hold.vtxo_id == id && hold.reason == "Lightning settlement provenance unknown" { return; }
			}
		}).await.expect("the decision must re-read the settlement and hold the source");
		assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1",
			&[&id.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0, "a settled hash must never refund its sender");
		return;
	}
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(source_expiry.to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let parent: Transaction = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if let Some((_, parent)) = super::expiry_test::published_payout(&db, id).await { break parent; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("failed send still requires an online client to refund it");
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	let (index, output) = parent.output.iter().enumerate().find(|(_, output)| output.script_pubkey == destination).unwrap();
	assert_eq!(output.value, sat(98_000));
	assert!(super::expiry_test::actual_fee(&ctx, &parent).await > bitcoin::Amount::ZERO);
	ctx.generate_blocks(6).await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
	let upstream = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, id).await).await.unwrap().unwrap();
	assert_eq!(upstream.htlc.resolution(), None, "fallback must not manufacture a cooperative user revocation");
	assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM lightning_payment_attempt_htlc_vtxo \
		WHERE lightning_payment_attempt_id=$1 AND vtxo_id=$2", &[&attempt, &id.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 1);
}

/// A settlement that commits between the decision's analysis and its write
/// makes the write refuse the sender's refund; the source then stays held.
#[tokio::test]
async fn expiry_settlement_during_decision_keeps_source_held() {
	failed_external_payout(true).await;
}

#[inline(never)]
fn internal_zero_grant_timeout() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
	Box::pin(async move {
	let name = "server/expiry_internal_zero_grant_timeout";
	let ctx = TestContext::new(name).await;
	let lightning = ctx.new_lightning_setup_no_channel("lightningd").await;
	let builder = ctx.captaind("server").lightningd(&lightning.internal).bitcoind(ctx.bitcoind_arc());
	let builder = builder.no_vtxo_pool();
	let srv = builder
		.funded(btc(10)).cfg(move |cfg| {
			cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(400);
			cfg.receive_htlc_forward_timeout = Duration::from_secs(2);
			cfg.invoice_check_interval = Duration::from_secs(1);
		}).watchmand().create().await;
	let recipient = ctx.bark_sdk("absent-recipient", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let invoice_amount = 90_000;
	let invoice = recipient.bolt11_invoice(sat(invoice_amount), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	drop(recipient); // No prepare-claim, claim, revoke or later wallet call.
	let sender = ctx.bark("absent-sender", &srv).funded(sat(200_000)).create().await;
	let ids = sender.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = sender.client().await;
	let input = client.get_full_vtxo(ids[0]).await.unwrap();
	let source_expiry = input.expiry_height();
	assert_eq!(input.amount(), sat(100_000));
	let key = client.get_vtxo_key(&input).await.unwrap();
	let destination = client.fallback_script();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let source = super::expiry_send_claim::signed_leaf(&ctx, &srv, &client, input,
		VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).await.id();
	drop(client);
	lightning.sync().await;
	let mut rpc = srv.get_public_rpc().await;
	rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
		protos::InitiateLightningPaymentRequest {
		invoice: invoice.to_string(), payment_amount_sat: invoice_amount,
		htlc_vtxo_ids: vec![source.to_bytes().to_vec()], mailbox_id: None, retry_for_secs: Some(5),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	}, &key).await).await.unwrap();
	drop(rpc); // No second prepare, cooperative revoke, claim or wallet return.
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (attempt, subscription) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_opt("SELECT a.id,a.lightning_htlc_subscription_id AS subscription_id FROM lightning_payment_attempt a \
				JOIN lightning_htlc_subscription s ON s.id=a.lightning_htlc_subscription_id AND s.payment_hash=a.payment_hash \
				WHERE a.payment_hash=$1 AND a.status='failed' AND s.status='canceled'", &[&hash.to_string()]).await?)).await.unwrap();
			if let Some(row) = row { break (row.get::<_, i64>("id"), row.get::<_, i64>("subscription_id")); }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("normal accepted-setup timeout did not cancel the actual internal subscription");
	let mut hold = lightning.internal.hold_client().await;
	let actual = hold.list(hold::ListRequest {
		constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner();
	assert!(actual.invoices.iter().any(|i| i.payment_hash == hash.to_vec()
		&& i.invoice == invoice.to_string() && i.state == hold::InvoiceState::Cancelled as i32));
	let mut node = lightning.internal.grpc_client().await;
	assert!(node.list_send_pays(cln_rpc::ListsendpaysRequest {
		payment_hash: Some(hash.to_vec()), ..Default::default()
	}).await.unwrap().into_inner().payments.is_empty());
	let unclaimed = db.read(async |t| Ok(t.query_one("SELECT \
		(SELECT COUNT(*) FROM vtxo WHERE lightning_htlc_subscription_id=$1) AS grants, \
		(SELECT COUNT(*) FROM expiry_lightning_receive_claim WHERE subscription_id=$1) AS bindings, \
		EXISTS(SELECT 1 FROM htlc_settlement WHERE payment_hash=$2) AS wal",
		&[&subscription,&hash.to_string()]).await?)).await.unwrap();
	assert_eq!(unclaimed.get::<_, i64>("grants"), 0);
	assert_eq!(unclaimed.get::<_, i64>("bindings"), 0);
	assert!(!unclaimed.get::<_, bool>("wal"));
	let fee_address = srv.watchmand().wait_wallet_address().await;
	// Each concurrent parent needs its own CONFIRMED fee input at the held tip.
	let fee_inputs = 1;
	for _ in 0..fee_inputs {
		ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
	}
	ctx.generate_blocks(2).await;
	let height = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(source_expiry.to_u32() + 1 - height).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let parent: Transaction = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			if let Some((_, parent)) = super::expiry_test::published_payout(&db, source).await { return parent; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("cancelled zero-grant internal funding still requires a returning wallet");
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	let (index, paid) = parent.output.iter().enumerate().find(|(_, o)| o.script_pubkey == destination).unwrap();
	assert_eq!(paid.value, sat(98_000));
	assert_eq!(parent.output.iter().filter(|o| o.script_pubkey == destination).count(), 1);
	assert!(super::expiry_test::actual_fee(&ctx, &parent).await > bitcoin::Amount::ZERO);
	ctx.generate_blocks(6).await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
	let kept = db.read(async |t| Ok(t.query_one("SELECT \
		EXISTS(SELECT 1 FROM lightning_payment_attempt WHERE id=$1 AND lightning_htlc_subscription_id=$2) AS funding, \
		(SELECT COUNT(*) FROM vtxo WHERE lightning_htlc_subscription_id=$2) AS grants, \
		(SELECT COUNT(*) FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id \
			WHERE e.vtxo_id=$3 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='customer') AS paid_once",
		&[&attempt,&subscription,&source.to_string()]).await?)).await.unwrap();
	assert!(kept.get::<_, bool>("funding"));
	assert_eq!(kept.get::<_, i64>("grants"), 0);
	assert_eq!(kept.get::<_, i64>("paid_once"), 1);
	let original = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, source).await).await.unwrap().unwrap();
	assert_eq!(original.htlc.resolution(), None, "automatic closure never invents a signed revocation");
	})
}

#[tokio::test]
async fn exposed_small_receive_grant_refunds_absent_sender_after_six_confirmations() {
	tokio::time::timeout(Duration::from_secs(240), internal_safe_pool_grants(true)).await
		.expect("exposed grant closure and sender refund timed out");
}

#[tokio::test]
async fn expired_internal_source_waits_for_six_confirmed_grant_reclamations() {
	internal_safe_pool_grants(false).await;
}

async fn assert_internal_source_held(db: &Db, source: VtxoId) {
	let row = db.read(async |t| Ok(t.query_one("SELECT v.spend_state::TEXT,
		EXISTS(SELECT 1 FROM expiry_entitlement e WHERE e.vtxo_id=v.vtxo_id) AS entitled,
		EXISTS(SELECT 1 FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id
			WHERE e.vtxo_id=v.vtxo_id AND c.disposition='customer') AS allocated
		FROM vtxo v WHERE v.vtxo_id=$1", &[&source.to_string()]).await?)).await.unwrap();
	assert_eq!(row.get::<_, &str>("spend_state"), "spendable");
	assert!(!row.get::<_, bool>("entitled"), "a live internal grant must not fence its expired funding source");
	assert!(!row.get::<_, bool>("allocated"), "a live internal grant must prevent a sender payout");
}

#[inline(never)]
fn internal_safe_pool_grants(expose_small_grant: bool)
	-> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>
{
	Box::pin(async move {
	let past_source_expiry = !expose_small_grant;
	let name = if expose_small_grant { "server/exposed_small_receive_grant" }
		else { "server/expired_internal_source_grant_closure" };
	let ctx = TestContext::new(name).await;
	let lightning = ctx.new_lightning_setup_no_channel("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).bitcoind(ctx.bitcoind_arc())
		.funded(btc(10)).cfg(move |cfg| { cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(400);
			cfg.vtxopool.vtxo_lifetime = BlockDelta::new(24);
			cfg.vtxopool.vtxo_pre_expiry = BlockDelta::new(2);
			cfg.vtxopool.vtxo_targets = if expose_small_grant {
				vec![server::vtxopool::VtxoTarget { amount: sat(45_000), count: 4 }]
			} else { vec![server::vtxopool::VtxoTarget { amount: sat(50_000), count: 4 }] };
			cfg.invoice_check_interval = Duration::from_secs(1);
		}).watchmand().create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let recipient = ctx.bark_sdk("absent-recipient", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	recipient.stop_daemon_wait().await.unwrap();
	let invoice_amount = if expose_small_grant { 90_330 } else { 90_000 };
	let invoice = recipient.bolt11_invoice(sat(invoice_amount), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	let receiver_keypair = super::receive_claim_keypair(&srv, &recipient, hash).await;
	let receiver_key = receiver_keypair.public_key();
	let receiver_destination = recipient.fallback_script();
	let sender = ctx.bark("absent-sender", &srv).funded(sat(200_000)).create().await;
	let ids = sender.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = sender.client().await;
	let input = client.get_full_vtxo(ids[0]).await.unwrap();
	let source_expiry = input.expiry_height();
	assert_eq!(input.amount(), sat(100_000));
	let key = client.get_vtxo_key(&input).await.unwrap();
	let destination = client.fallback_script();
	assert_ne!(destination, receiver_destination);
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let incoming_deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let outgoing_deadline = tip + BlockDelta::new(36);
	let incoming = vec![super::expiry_send_claim::signed_leaf(&ctx, &srv, &client, input,
		VtxoPolicy::new_server_htlc_send(key.public_key(), hash, incoming_deadline)).await];
	let source = incoming[0].id();
	let mut rpc = srv.get_public_rpc().await;
	lightning.sync().await;
	rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
		protos::InitiateLightningPaymentRequest {
		invoice: invoice.to_string(), payment_amount_sat: invoice_amount,
		htlc_vtxo_ids: vec![source.to_bytes().to_vec()], mailbox_id: None, retry_for_secs: Some(5),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	}, &key).await).await.unwrap();
	let granted = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: receiver_key.serialize().to_vec(),
		htlc_recv_expiry: outgoing_deadline.to_u32(), lightning_receive_anti_dos: None,
		claim_attestation: Some(ark::attestations::LightningReceiveClaimAttestation::new(hash, outgoing_deadline, &receiver_keypair).serialize()),
	}).await.unwrap().into_inner();
	let grants = granted.htlc_vtxos.iter().map(|bytes| Vtxo::<Full>::deserialize(bytes).unwrap()).collect::<Vec<_>>();
	assert_eq!(grants.len(), if expose_small_grant { 3 } else { 2 },
		"admission must retain the standard existing cover, including the normal two-grant control");
	assert_eq!(grants.iter().map(|g| g.amount()).sum::<bitcoin::Amount>(), sat(invoice_amount));
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	assert_stored_grant_ancestry(&db, &grants).await;
	let fee_address = srv.watchmand().wait_wallet_address().await;
	if expose_small_grant {
		srv.watchmand().stop().await.unwrap();
		let small = grants.iter().find(|grant| grant.amount() == sat(330)).unwrap();
		assert_eq!(small.amount(), small.output_script_pubkey().minimal_non_dust());
		let mut published = std::collections::BTreeSet::new();
		for grant in &grants {
			super::expiry_test::confirm_actual_path(&ctx, grant, &mut published).await;
			let output = ctx.bitcoind().sync_client().get_tx_out(&grant.point().txid, grant.point().vout, Some(true))
				.unwrap().unwrap();
			assert!(output.confirmations > 0);
			assert_eq!(output.value, grant.amount());
			assert_eq!(output.script_pub_key.script().unwrap(), grant.output_script_pubkey());
		}
	}
	drop(client);
	drop(recipient);
	drop(rpc); // No preimage disclosure, cooperative revoke, or returning wallet.
	assert!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	if past_source_expiry {
		let height = ctx.bitcoind().get_block_count().await as u32;
		let height = ctx.generate_blocks(source_expiry.to_u32() + 1 - height).await;
		srv.wait_for_sync_height(height).await;
		srv.watchmand().wait_for_sync_height(height).await;
		tokio::time::timeout(Duration::from_secs(15), async {
			loop {
				let ready = db.read(async |t| {
					let row = t.query_one("SELECT a.status::TEXT,s.status::TEXT AS subscription_status
					FROM lightning_payment_attempt a JOIN lightning_htlc_subscription s ON s.id=a.lightning_htlc_subscription_id
					JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id
					WHERE l.vtxo_id=$1", &[&source.to_string()]).await?;
					Ok(row.get::<_, &str>("status") == "failed" && row.get::<_, &str>("subscription_status") == "canceled"
						&& t.expiry_view().await?.is_some())
				}).await.unwrap();
				if ready { break; }
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
		}).await.expect("expired uncollected internal attempt must become Failed");
		tokio::time::sleep(Duration::from_secs(2)).await;
		assert_internal_source_held(&db, source).await;
	}
	ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
	ctx.generate_blocks(2).await;
	if past_source_expiry {
		let ids = grants.iter().map(|grant| grant.id().to_string()).collect::<Vec<_>>();
		let closure_height = tokio::time::timeout(Duration::from_secs(45), async {
			loop {
				let height = ctx.generate_blocks(1).await;
				srv.wait_for_sync_height(height).await;
				srv.watchmand().wait_for_sync_height(height).await;
				let row = db.read(async |t| Ok(t.query_one("SELECT COUNT(DISTINCT e.vtxo_id) AS reclaimed,MAX(a.confirmed_height) AS height
					FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id
					JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE e.vtxo_id=ANY($1)
					AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='operator' AND a.status='confirmed'", &[&ids]).await?)).await.unwrap();
				if row.get::<_, i64>("reclaimed") == grants.len() as i64 { break u32::try_from(row.get::<_, i64>("height")).unwrap(); }
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
		}).await.expect("uncollected internal grants must be reclaimed after fee replenishment");
		assert_grants_reclaimed(&ctx, &db, &grants, &receiver_destination).await;
		let height = ctx.bitcoind().get_block_count().await as u32;
		assert!(height < closure_height + 5);
		let height = ctx.generate_blocks(closure_height + 4 - height).await;
		srv.wait_for_sync_height(height).await;
		srv.watchmand().wait_for_sync_height(height).await;
		tokio::time::sleep(Duration::from_secs(1)).await;
		assert_internal_source_held(&db, source).await;
		ctx.generate_blocks(1).await;
	}
	if expose_small_grant {
		let chain = ctx.bitcoind().sync_client();
		let mut maturity = outgoing_deadline.to_u32();
		for grant in &grants {
			let output = chain.get_tx_out(&grant.point().txid, grant.point().vout, Some(true)).unwrap().unwrap();
			let confirmed = chain.get_block_count().unwrap() as u32 + 1 - output.confirmations as u32;
			maturity = maturity.max(confirmed + grant.exit_delta().to_u32()).max(grant.expiry_height().to_u32());
		}
		let height = chain.get_block_count().unwrap() as u32;
		ctx.generate_blocks(maturity.saturating_sub(height)).await;
		srv.watchmand().start().await.unwrap();
		let ids = grants.iter().map(|g| g.id().to_string()).collect::<Vec<_>>();
		let closures = tokio::time::timeout(Duration::from_secs(45), async {
			loop {
				let height = ctx.generate_blocks(1).await;
				srv.wait_for_sync_height(height).await;
				srv.watchmand().wait_for_sync_height(height).await;
				let revoked = db.read(async |t| Ok(t.query_one(
					"SELECT COUNT(*) FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id
					 WHERE v.vtxo_id=ANY($1) AND h.chain_resolution='revoked'", &[&ids]).await?
					.get::<_, i64>(0))).await.unwrap();
				if revoked == grants.len() as i64 {
					if let Some(closures) = confirmed_grant_closures(&ctx, &db, &grants, &receiver_destination).await {
						break closures;
					}
				}
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
		}).await.expect("exposed receive grants must close through native timeout claims");
		let small = grants.iter().find(|grant| grant.amount() == sat(330)).unwrap();
		let closure = ctx.bitcoind().await_transaction(closures[&small.id()].0).await;
		assert!(super::expiry_test::actual_fee(&ctx, &closure).await > bitcoin::Amount::ZERO,
			"the native claim pays its actual miner fee from operator funds");
		assert_eq!(closure.version, bitcoin::transaction::Version::TWO);
		assert!(closure.output.iter().all(|o| o.script_pubkey != receiver_destination));
		let operator_script = srv.watchmand().config().sweep_address.clone().unwrap().assume_checked().script_pubkey();
		assert!(closure.output.iter().all(|output| output.script_pubkey == operator_script),
			"native operator reclamation cannot redirect principal to another recipient");
		let depth = chain.get_block_count().unwrap() as u32 + 1 - closures.values().map(|(_, height)| *height).max().unwrap();
		assert!(depth < 6);
		let height = ctx.generate_blocks(5 - depth).await;
		srv.wait_for_sync_height(height).await;
		srv.watchmand().wait_for_sync_height(height).await;
		let point = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
		super::expiry_test::wait_rebuilt(&db, point, Duration::from_secs(10),
			"grant closure view must finish at five confirmations").await;
		tokio::time::sleep(Duration::from_secs(1)).await;
		assert!(db.read(async |t| Ok(t.query_opt(
			"SELECT c.allocation_id FROM expiry_allocation_coin c
			 JOIN expiry_entitlement e ON e.id = c.entitlement_id
			 WHERE e.vtxo_id = $1 AND c.disposition = 'customer'",
			&[&source.to_string()]).await?)).await.unwrap().is_none(), "sender refund must wait for six grant confirmations");
		let fact = db.read(async |t| {
			let snapshot = t.backing_subtree_snapshot(&[ark::ServerVtxo::from(incoming[0].clone()).to_bare()]).await?;
			Ok(snapshot.regular.into_iter().find(|fact| fact.state.vtxo_id == source).unwrap())
		}).await.unwrap();
		let funding = ctx.bitcoind().await_transaction(incoming[0].chain_anchor().txid).await;
		let proposal = db.read(async |t| t.propose_expiry_ownership(&fact, height).await).await.unwrap();
		assert!(matches!(proposal.disposition, server::database::expiry_ownership::OwnershipDisposition::Unresolved(_)));
		let height = ctx.generate_blocks(1).await;
		srv.wait_for_sync_height(height).await;
		srv.watchmand().wait_for_sync_height(height).await;
		let mut least_depth = u32::MAX;
		for grant in &grants {
			let row = db.read(async |t| Ok(t.query_one("SELECT chain_resolution::TEXT,chain_resolution_height \
				FROM htlc_vtxo h JOIN vtxo v ON v.id=h.id WHERE v.vtxo_id=$1", &[&grant.id().to_string()]).await?)).await.unwrap();
			assert_eq!(row.get::<_, &str>("chain_resolution"), "revoked");
			let resolved = u32::try_from(row.get::<_, i32>("chain_resolution_height")).unwrap();
			least_depth = least_depth.min(height.to_u32() - resolved + 1);
		}
		assert_eq!(least_depth, 6);
		let point = server::bitcoind::tip(&ctx.bitcoind().async_client()).await.unwrap();
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let proposal = db.read(async |t| {
					let Some(view) = t.expiry_view().await? else { return Ok(None); };
					if view != point { return Ok(None); }
					Ok(Some(t.propose_expiry_ownership(&fact, view.height).await?))
				}).await.unwrap();
				if let Some(proposal) = proposal {
					if matches!(proposal.disposition, server::database::expiry_ownership::OwnershipDisposition::Customer { user_pubkey } if user_pubkey == key.public_key()) {
						assert!(matches!(proposal.evidence,
							Some(server::database::expiry_ownership::OwnershipEvidence::HtlcInternalGrantsReclaimed { payment_hash, .. }) if payment_hash == hash));
						break;
					}
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("at N6 the actual revoked grants must release their exact failed funding source");
	}
	// Reach the original 100-block reaction window for this long-lived source.
	if !past_source_expiry {
		let height = ctx.bitcoind().get_block_count().await as u32;
		let target = source_expiry.saturating_sub(BlockDelta::new(40)).to_u32();
		if target > height {
			let height = ctx.generate_blocks(target - height).await;
			srv.wait_for_sync_height(height).await;
			srv.watchmand().wait_for_sync_height(height).await;
		}
	}
	// Normal watchman progress has to reach every HTLC leaf. Mine only
	// through completed sync cycles and leave reaction/readiness time each block.
	let parent: Transaction = tokio::time::timeout(Duration::from_secs(150), async {
		for _ in 0..100 {
			let height = ctx.generate_blocks(1).await;
			srv.wait_for_sync_height(height).await;
			srv.watchmand().wait_for_sync_height(height).await;
			tokio::time::sleep(Duration::from_secs(1)).await;
			if let Some((_, parent)) = super::expiry_test::published_payout(&db, source).await { return parent; }
		}
		panic!("the absent sender was not refunded after its receive grants were reclaimed");
	}).await.expect("both-absent internal timeout required a returning wallet");
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	assert_grants_reclaimed(&ctx, &db, &grants, &receiver_destination).await;
	let (index,paid) = parent.output.iter().enumerate().find(|(_, o)| o.script_pubkey == destination).unwrap();
	assert_eq!(paid.value, sat(98_000));
	assert!(parent.output.iter().all(|o| o.script_pubkey != receiver_destination));
	assert!(super::expiry_test::actual_fee(&ctx, &parent).await > bitcoin::Amount::ZERO);
	ctx.generate_blocks(6).await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
	let chain = ctx.bitcoind().sync_client();
	let mut outputs = Vec::new();
	for height in u64::from(tip.to_u32())..=chain.get_block_count().unwrap() {
		for tx in chain.get_block(&chain.get_block_hash(height).unwrap()).unwrap().txdata {
			for (vout, output) in tx.output.iter().enumerate() {
				assert_ne!(output.script_pubkey, receiver_destination);
				if output.script_pubkey == destination {
					outputs.push((bitcoin::OutPoint::new(tx.compute_txid(), vout as u32), output.value));
				}
			}
		}
	}
	assert_eq!(outputs, vec![(bitcoin::OutPoint::new(parent.compute_txid(), index as u32), sat(98_000))]);
	for txid in chain.get_raw_mempool().unwrap() {
		let tx = ctx.bitcoind().await_transaction(txid).await;
		assert!(tx.output.iter().all(|output|
			output.script_pubkey != destination && output.script_pubkey != receiver_destination));
	}
	assert!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());

	let state = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, source).await).await.unwrap().unwrap();
	assert!(state.htlc.offchain_resolution.is_none(), "automatic timeout refund must not manufacture the absent user's revocation");
	})
}

async fn matching_htlcs(
	node: &ark_testing::daemon::lightningd::Lightningd, hash: PaymentHash,
) -> Vec<cln_rpc::ListpeerchannelsChannelsHtlcs> {
	let mut channels = node.grpc_client().await.list_peer_channels(cln_rpc::ListpeerchannelsRequest {
		id: None, short_channel_id: None, channel_id: None,
	}).await.unwrap().into_inner().channels;
	assert_eq!(channels.len(), 1);
	assert_eq!(channels[0].state(), cln_rpc::ChannelState::ChanneldNormal);
	channels.remove(0).htlcs.into_iter().filter(|part| part.payment_hash == hash.to_vec()).collect()
}

/// A genuine native Pending source must not block an earlier unrelated payout.
/// Actual receiver cancellation and peer release precede its offline refund.
#[tokio::test]
async fn pending_external_send_allows_unrelated_payout_then_refunds_offline_once() {
	tokio::time::timeout(Duration::from_secs(180), Box::pin(async {
		let ctx = TestContext::new("server/pending_unrelated_offline_refund").await;
		let lightning = ctx.new_lightning_setup("lightningd").await;
		let srv = ctx.captaind("server").lightningd(&lightning.internal).no_vtxo_pool().funded(btc(10))
			.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(400); })
			.watchmand().create().await;
		let customer = ctx.bark_sdk("earlier-owner", &srv).cfg(|cfg| cfg.daemon_manual_sync = true)
			.funded(sat(200_000)).boarded(sat(100_000)).create().await;
		customer.stop_daemon_wait().await.unwrap();
		let coins = customer.spendable_vtxos().await.unwrap();
		assert_eq!(coins.len(), 1);
		let earlier = customer.get_full_vtxo(coins[0].id()).await.unwrap();
		let earlier_destination = customer.fallback_script();
		let earlier_mailbox = customer.mailbox_identifier();
		assert_eq!(earlier.amount(), sat(100_000));

		// Age the first backing before any payment; both use the same400-block policy.
		let before_source = earlier.expiry_height().to_u32().checked_sub(64).unwrap();
		assert!((ctx.bitcoind().get_block_count().await as u32) < before_source);
		while (ctx.bitcoind().get_block_count().await as u32) < before_source {
			ctx.generate_blocks((before_source - ctx.bitcoind().get_block_count().await as u32).min(50)).await;
		}
		let sender = ctx.bark_sdk("later-owner", &srv).cfg(|cfg| cfg.daemon_manual_sync = true)
			.funded(sat(200_000)).boarded(sat(100_000)).create().await;
		sender.stop_daemon_wait().await.unwrap();
		let coins = sender.spendable_vtxos().await.unwrap();
		assert_eq!(coins.len(), 1);
		let input = sender.get_full_vtxo(coins[0].id()).await.unwrap();
		let key = sender.get_vtxo_key(&input).await.unwrap();
		let destination = sender.fallback_script();
		let mailbox = sender.mailbox_identifier();
		assert_ne!(mailbox, earlier_mailbox);
		assert_ne!(destination, earlier_destination);
		let preimage = Preimage::random();
		let hash = preimage.compute_payment_hash();
		let admission_tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
		let deadline = admission_tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
		let source = super::expiry_send_claim::signed_leaf(&ctx, &srv, &sender, input,
			VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).await;
		assert_eq!(source.amount(), sat(100_000));
		assert_ne!(source.chain_anchor(), earlier.chain_anchor());
		assert!(admission_tip < earlier.expiry_height());
		assert!(earlier.expiry_height() < source.expiry_height());
		assert!(deadline < source.expiry_height());
		let db = Db::connect(&srv.config().postgres).await.unwrap();
		for (coin, owner) in [(&source, mailbox), (&earlier, earlier_mailbox)] {
			let funding = ctx.bitcoind().await_transaction(coin.chain_anchor().txid).await;
			coin.validate(&funding).unwrap();
			db.read(async |tx| {
				assert_eq!(tx.get_user_vtxo_by_id(coin.id()).await?.vtxo.serialize(), coin.serialize());
				assert_eq!(tx.coin_owner_mailboxes(&[coin.id()]).await?.get(&coin.id()), Some(&owner.as_pubkey()));
				Ok(())
			}).await.unwrap();
		}
		// Only the original100k subsidy. Its first confirmed change sponsors the second payout.
		let fee_address = srv.watchmand().wait_wallet_address().await;
		assert_ne!(fee_address.script_pubkey(), destination);
		assert_ne!(fee_address.script_pubkey(), earlier_destination);
		let sponsor = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
		let sponsor_tx = ctx.bitcoind().await_transaction(sponsor).await;
		assert_eq!(sponsor_tx.output.iter().filter(|o|
			o.script_pubkey == fee_address.script_pubkey() && o.value == sat(100_000)).count(), 1);
		ctx.generate_blocks(2).await;
		let invoice = lightning.external.hold_client().await.invoice(hold::InvoiceRequest {
			payment_hash: hash.to_vec(), amount_msat: 90_000_000,
			description: Some(hold::invoice_request::Description::Memo("pending isolation then offline refund".into())),
			min_final_cltv_expiry: Some(220), expiry: Some(3600), routing_hints: vec![],
		}).await.unwrap().into_inner().bolt11;
		lightning.sync().await;
		srv.get_public_rpc().await.initiate_lightning_payment(
			ark_testing::lightning::sign_lightning_send_request(&srv, protos::InitiateLightningPaymentRequest {
			invoice, payment_amount_sat: 90_000, htlc_vtxo_ids: vec![source.id().to_bytes().to_vec()],
			mailbox_id: None, retry_for_secs: Some(5),
			intent_nonce: None, source_owner_signatures: Vec::new(),
		}, &key).await).await.unwrap();
		lightning.external.wait_for_hold_invoice_accepted(hash).await;
		let payer = lightning.internal.grpc_client().await.getinfo(cln_rpc::GetinfoRequest {}).await.unwrap().into_inner();
		let receiver = lightning.external.grpc_client().await.getinfo(cln_rpc::GetinfoRequest {}).await.unwrap().into_inner();
		assert_ne!(payer.id, receiver.id);
		let attempt = db.read(async |tx| {
			let rows = tx.query("SELECT a.id,a.amount_msat,n.pubkey FROM lightning_payment_attempt a
				JOIN lightning_node n ON n.id=a.lightning_node_id WHERE a.payment_hash=$1", &[&hash.to_string()]).await?;
			assert_eq!(rows.len(), 1);
			assert_eq!(rows[0].get::<_, i64>("amount_msat"), 90_000_000);
			assert_eq!(rows[0].get::<_, Vec<u8>>("pubkey"), payer.id);
			let id = rows[0].get::<_, i64>("id");
			let linked = tx.query("SELECT vtxo_id FROM lightning_payment_attempt_htlc_vtxo WHERE lightning_payment_attempt_id=$1", &[&id]).await?;
			assert_eq!(linked.len(), 1);
			assert_eq!(linked[0].get::<_, String>(0), source.id().to_string());
			Ok(id)
		}).await.unwrap();
		let label = ark_testing::lightning::attempt_xpay_label(&db, attempt).await;
		let peer_parts = async || {
			let tip = ctx.bitcoind().get_block_count().await as u32;
			let mut both = Vec::new();
			for (node, outgoing) in [(&lightning.internal, true), (&lightning.external, false)] {
				let matching = matching_htlcs(node, hash).await;
				assert!(!matching.is_empty());
				assert!(matching.iter().all(|p| p.expiry > tip && p.direction() == if outgoing {
					cln_rpc::listpeerchannels_channels_htlcs::ListpeerchannelsChannelsHtlcsDirection::Out
				} else { cln_rpc::listpeerchannels_channels_htlcs::ListpeerchannelsChannelsHtlcsDirection::In }));
				assert_eq!(matching.iter().map(|p| p.amount_msat.as_ref().unwrap().msat).sum::<u64>(), 90_000_000);
				let mut ids = matching.iter().map(|p| (p.id, p.expiry)).collect::<Vec<_>>();
				ids.sort_unstable();
				both.push(ids);
			}
			both
		};
		let pending_cut = async || {
			let history = super::expiry_send_claim::send_history(&lightning.internal, hash).await;
			assert!(!history.0.is_empty());
			assert!(history.0.iter().all(|p| p.label.as_deref() == Some(label.as_str())
				&& p.status() == cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Pending
				&& p.created_index > 0 && p.payment_preimage.is_none()));
			assert_eq!(history.0.iter().map(|p| (p.groupid, p.partid)).collect::<std::collections::BTreeSet<_>>().len(), history.0.len());
			assert_eq!(history.0.iter().map(|p| p.groupid).collect::<std::collections::BTreeSet<_>>().len(), 1);
			assert_eq!(history.0.iter().map(|p| p.amount_msat.as_ref().unwrap().msat).sum::<u64>(), 90_000_000);
			assert_eq!(history.0.iter().map(|p| p.amount_sent_msat.as_ref().unwrap().msat).sum::<u64>(), 90_000_000);
			assert_eq!(history.1.len(), 1);
			assert_eq!(history.1[0].label.as_deref(), Some(label.as_str()));
			assert_eq!(history.1[0].status(), cln_rpc::listpays_pays::ListpaysPaysStatus::Pending);
			assert_eq!(history.1[0].amount_msat.as_ref().unwrap().msat, 90_000_000);
			assert_eq!(history.1[0].amount_sent_msat.as_ref().unwrap().msat, 90_000_000);
			assert!(history.1[0].created_index.unwrap() > 0);
			assert!(history.1[0].preimage.is_none());
			db.read(async |tx| {
				assert!(tx.query_one("SELECT status NOT IN ('failed','succeeded') FROM lightning_payment_attempt WHERE id=$1",
					&[&attempt]).await?.get::<_, bool>(0));
				assert_eq!(tx.query_one("SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1",
					&[&source.id().to_string()]).await?.get::<_, i64>(0), 0);
				assert!(tx.get_htlc_settlement_by_payment_hash(hash).await?.is_none());
				Ok(())
			}).await.unwrap();
			let status = srv.get_public_rpc().await.check_lightning_payment(protos::CheckLightningPaymentRequest {
				hash: hash.to_vec(), wait: false, htlc_vtxo_ids: vec![source.id().to_bytes().to_vec()],
			}).await.unwrap().into_inner();
			assert_eq!(status.htlc_vtxo_ids, vec![source.id().to_bytes().to_vec()]);
			assert!(matches!(status.payment_status, Some(protos::lightning_payment_status::PaymentStatus::Pending(_))));
			let anchor = source.chain_anchor();
			let funding = ctx.bitcoind().await_transaction(anchor.txid).await;
			let backing = ctx.bitcoind().sync_client().get_tx_out(&anchor.txid, anchor.vout, Some(true)).unwrap().unwrap();
			assert_eq!(backing.value, source.amount());
			assert_eq!(backing.script_pub_key.script().unwrap(), funding.output[anchor.vout as usize].script_pubkey);
			(history, peer_parts().await)
		};
		let initial = pending_cut().await;
		let native_deadline = initial.1.iter().flatten().map(|(_, expiry)| *expiry).min().unwrap();
		assert!(earlier.expiry_height().to_u32() + 1 + 6 < native_deadline);
		drop(sender);
		drop(customer);
		let absent_height = ctx.bitcoind().get_block_count().await;
		let payout = async |coin: &Vtxo<Full>, script: &bitcoin::ScriptBuf| {
			let mut prefixes = coin.transactions().map(|s| s.tx.compute_txid()).collect::<std::collections::BTreeSet<_>>();
			tokio::time::timeout(Duration::from_secs(30), async {
				loop {
					if let Some((_, parent)) = super::expiry_test::published_payout(&db, coin.id()).await {
						let rightful = parent.output.iter().filter(|o| &o.script_pubkey == script).collect::<Vec<_>>();
						assert_eq!(rightful.len(), 1);
						assert_eq!(rightful[0].value, sat(98_000));
						assert!(parent.output.iter().all(|o| &o.script_pubkey == script
							|| (o.script_pubkey != destination && o.script_pubkey != earlier_destination)));
						if ctx.bitcoind().sync_client().get_raw_transaction(&parent.compute_txid(), None).ok().as_ref() == Some(&parent) {
							let fee = super::expiry_test::actual_fee(&ctx, &parent).await;
							assert!(fee > bitcoin::Amount::ZERO && fee < sat(100_000));
							return parent;
						}
					}
					if let Some(txid) = ctx.bitcoind().sync_client().get_raw_mempool().unwrap().into_iter().find(|id| prefixes.contains(id)) {
						assert!(prefixes.remove(&txid));
						ctx.generate_blocks(srv.watchmand().config().watchman.progress_grace_period.to_u32() + 1).await;
						super::expiry_test::rebuilt(&ctx, &srv, &db).await;
					}
					tokio::time::sleep(Duration::from_millis(50)).await;
				}
			}).await.expect("ordinary offline payout must be admitted without another sponsor or client")
		};
		let tip = ctx.bitcoind().get_block_count().await as u32;
		ctx.generate_blocks(earlier.expiry_height().to_u32() + 1 - tip).await;
		super::expiry_test::rebuilt(&ctx, &srv, &db).await;
		let first = payout(&earlier, &earlier_destination).await;
		ctx.generate_blocks(6).await;
		super::expiry_test::rebuilt(&ctx, &srv, &db).await;
		super::expiry_test::canonical(&ctx, &first);
		let output_index = first.output.iter().position(|o| o.script_pubkey == earlier_destination && o.value == sat(98_000)).unwrap();
		let core = ctx.bitcoind().sync_client();
		let actual = core.get_tx_out(&first.compute_txid(), output_index as u32, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 6);
		assert_eq!(actual.value, sat(98_000));
		assert_eq!(actual.script_pub_key.script().unwrap(), earlier_destination);
		assert!(pending_cut().await == initial,
			"unrelated N6 payout must leave every native part/group and both peers' future HTLC identities unchanged");

		lightning.external.hold_client().await.cancel(hold::CancelRequest { payment_hash: hash.to_vec() }).await.unwrap();
		let failed = tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				let history = super::expiry_send_claim::send_history(&lightning.internal, hash).await;
				let native_failed = !history.0.is_empty()
					&& history.0.iter().all(|p| p.status() == cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Failed)
					&& history.1.len() == 1 && history.1[0].status() == cln_rpc::listpays_pays::ListpaysPaysStatus::Failed;
				let recorded = db.read(async |tx| Ok(tx.query_one(
					"SELECT status='failed' AND final_amount_msat=0 FROM lightning_payment_attempt WHERE id=$1",
					&[&attempt]).await?.get::<_, bool>(0))).await.unwrap();
				if native_failed && recorded {
					assert!(history.0.iter().all(|p| p.label.as_deref() == Some(label.as_str()) && p.payment_preimage.is_none()));
					assert_eq!(history.1[0].label.as_deref(), Some(label.as_str()));
					assert!(history.1[0].preimage.is_none());
					assert_eq!(history.1[0].amount_sent_msat.as_ref().unwrap().msat, 0);
					assert_eq!(history.0.iter().map(|p| (p.id, p.groupid, p.partid, p.created_index)).collect::<Vec<_>>(),
						initial.0.0.iter().map(|p| (p.id, p.groupid, p.partid, p.created_index)).collect::<Vec<_>>());
					assert_eq!(history.0.iter().map(|p| p.amount_msat.as_ref().unwrap().msat).sum::<u64>(), 90_000_000);
					break history;
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("ordinary receiver cancel must produce genuine same-attempt native failure");
		tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				let a = matching_htlcs(&lightning.internal, hash).await;
				let b = matching_htlcs(&lightning.external, hash).await;
				if a.is_empty() && b.is_empty() { break; }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("both native peers must actually release the matching HTLCs");
		assert!((ctx.bitcoind().get_block_count().await as u32) < deadline.to_u32());
		assert_eq!(db.read(async |tx| Ok(tx.query_one(
			"SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1",
			&[&source.id().to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
		let target = source.expiry_height().to_u32().max(deadline.to_u32()) + 1;
		while (ctx.bitcoind().get_block_count().await as u32) < target {
			ctx.generate_blocks((target - ctx.bitcoind().get_block_count().await as u32).min(50)).await;
		}
		super::expiry_test::rebuilt(&ctx, &srv, &db).await;
		let second = payout(&source, &destination).await;
		assert_ne!(first.compute_txid(), second.compute_txid());
		assert!(first.input.iter().all(|a| second.input.iter().all(|b| a.previous_output != b.previous_output)));
		ctx.generate_blocks(6).await;
		super::expiry_test::rebuilt(&ctx, &srv, &db).await;
		let expected = std::collections::BTreeMap::from([(earlier_destination.clone(), sat(98_000)), (destination.clone(), sat(98_000))]);
		let mut paid = std::collections::BTreeMap::new();
		let mut paid_by_script = std::collections::BTreeMap::new();
		for parent in [&first, &second] {
			super::expiry_test::canonical(&ctx, parent);
			for (vout, output) in parent.output.iter().enumerate().filter(|(_, o)| expected.contains_key(&o.script_pubkey)) {
				let point = bitcoin::OutPoint::new(parent.compute_txid(), vout as u32);
				let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
				assert!(actual.confirmations >= 6);
				assert_eq!(actual.value, output.value);
				assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
				assert!(paid.insert(point, output.value).is_none());
				assert!(paid_by_script.insert(output.script_pubkey.clone(), output.value).is_none());
			}
		}
		assert_eq!(paid_by_script, expected);
		let miner_fees = super::expiry_test::actual_fee(&ctx, &first).await + super::expiry_test::actual_fee(&ctx, &second).await;
		assert!(miner_fees > bitcoin::Amount::ZERO && miner_fees < sat(100_000));
		assert_eq!(paid.values().copied().sum::<bitcoin::Amount>(), sat(196_000));
		assert_eq!(sat(200_000), paid.values().copied().sum::<bitcoin::Amount>() + sat(4_000));
		for _ in 0..2 {
			ctx.generate_blocks(6).await;
			super::expiry_test::rebuilt(&ctx, &srv, &db).await;
			let mut actual = std::collections::BTreeMap::new();
			for height in absent_height + 1..=ctx.bitcoind().get_block_count().await {
				for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
					for (vout, output) in tx.output.iter().enumerate().filter(|(_, o)| expected.contains_key(&o.script_pubkey)) {
						assert!(actual.insert(bitcoin::OutPoint::new(tx.compute_txid(), vout as u32), output.value).is_none());
					}
				}
			}
			assert_eq!(actual, paid, "neither obligation may pay twice, including spent historical outputs");
			for txid in core.get_raw_mempool().unwrap() {
				let tx = core.get_raw_transaction(&txid, None).unwrap();
				assert!(tx.output.iter().all(|o| !expected.contains_key(&o.script_pubkey)));
			}
			for (point, value) in &paid {
				let output = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
				assert!(output.confirmations >= 6);
				assert_eq!(&output.value, value);
			}
			assert!(super::expiry_send_claim::send_history(&lightning.internal, hash).await == failed);
			assert!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
		}
		let stored = db.read(async |tx| server::database::htlc_vtxo::get_htlc_vtxo(tx, source.id()).await).await.unwrap().unwrap();
		assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1",
			&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 1);
		assert_eq!(stored.htlc.resolution(), None, "automatic fallback must not fabricate a cooperative revocation");
	})).await.expect("nativePending isolation, normal release and offline fallback must fit180s");
}
