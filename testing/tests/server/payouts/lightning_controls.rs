//! The Lightning controls payout mode adds to upstream: the decision fence at
//! initiation (F3) and at claim (F1), the acknowledged retry of a signed send
//! intent (A3), an open attempt on a canceled receive (A4), and the narrowed
//! stored-preimage hold (R6).

use std::sync::Arc;
use std::time::Duration;

use ark::{ProtocolEncoding, Vtxo, VtxoId, VtxoPolicy, SECP};
use ark::arkoor::package::ArkoorPackageBuilder;
use ark::attestations::LightningReceiveClaimAttestation;
use ark::lightning::{PaymentHash, Preimage};
use ark::mailbox::MailboxIdentifier;
use ark::vtxo::Full;
use ark_testing::{btc, sat, Captaind, TestContext};
use ark_testing::context::LightningPaymentSetup;
use ark_testing::lightning::sign_lightning_send_request;
use bitcoin::secp256k1::Keypair;
use bitcoin_ext::{BlockDelta, BlockHeight};
use cln_rpc::plugins::hold;
use server::database::Db;
use server::database::expiry_ownership::{OwnershipDisposition, OwnershipEvidence, OwnershipProposal};
use server_rpc::protos;

async fn payout_server(ctx: &TestContext, lightning: &LightningPaymentSetup) -> Arc<Captaind> {
	ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.expiry_payouts = true).create().await
}

/// A registered HTLC-send coin of a fresh wallet for `hash`, and its key.
async fn htlc_send_source(ctx: &TestContext, srv: &Captaind, hash: PaymentHash) -> (Vtxo<Full>, Keypair) {
	let sender = ctx.bark("sender", srv).funded(sat(500_000)).create().await;
	let ids = sender.board_and_confirm_and_register(ctx, sat(100_000)).await;
	let wallet = sender.client().await;
	let input = wallet.get_full_vtxo(ids[0]).await.unwrap();
	let key = wallet.get_vtxo_key(&input).await.unwrap();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let policy = VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline);
	(super::expiry_send_claim::signed_leaf(ctx, srv, &wallet, input, policy).await, key)
}

fn send_request(invoice: String, source: &Vtxo<Full>) -> protos::InitiateLightningPaymentRequest {
	protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: 100_000,
		htlc_vtxo_ids: vec![source.id().to_bytes().to_vec()],
		mailbox_id: None, retry_for_secs: Some(5),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	}
}

async fn attempt_count(db: &Db, hash: PaymentHash) -> i64 {
	db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1",
		&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap()
}

/// Wait until another connection waits on a row lock held by the test.
async fn wait_for_lock_waiter(db: &Db) {
	tokio::time::timeout(Duration::from_secs(30), async {
		while !db.read(async |t| Ok(t.query_one("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE NOT granted)", &[])
			.await?.get::<_, bool>(0))).await.unwrap() {
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the call never reached the row lock");
}

/// Decide `ids` for the expiry payout while `call` waits on their rows, so
/// the call only sees the decision in its own write.
async fn decide_during<T: Send + 'static>(
	db: &Db, ids: &[VtxoId], call: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
	let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
	let waiter = db.clone();
	db.write(async move |t| {
		t.query("SELECT vtxo_id FROM vtxo WHERE vtxo_id=ANY($1) FOR UPDATE", &[&ids]).await?;
		let call = tokio::spawn(call);
		wait_for_lock_waiter(&waiter).await;
		t.execute("INSERT INTO expiry_entitlement (vtxo_id) SELECT UNNEST($1::TEXT[])", &[&ids]).await?;
		Ok(call)
	}).await.unwrap().await.unwrap()
}

async fn proposal(db: &Db, source: &Vtxo<Full>, tip: BlockHeight) -> OwnershipProposal {
	let root = ark::ServerVtxo::from(source.clone()).to_bare();
	let snapshot = db.read(async |t| t.backing_subtree_snapshot(&[root]).await).await.unwrap();
	let fact = snapshot.regular.iter().find(|fact| fact.state.vtxo_id == source.id()).unwrap();
	db.read(async |t| t.propose_expiry_ownership(fact, tip).await).await.unwrap()
}

/// A refund pays the mailbox the source was born with.
async fn birth_owner(db: &Db, source: &Vtxo<Full>) -> OwnershipDisposition {
	let mailbox = db.read(async |t| t.coin_owner_mailboxes(&[source.id()]).await).await.unwrap()[&source.id()];
	OwnershipDisposition::Customer { mailbox }
}

/// F3: a source decided after its cosign is refused in the attempt write,
/// and no attempt is recorded.
#[tokio::test]
async fn source_decided_after_cosign_is_refused_at_initiation() {
	let ctx = TestContext::new("server/source_decided_after_cosign_refused").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = payout_server(&ctx, &lightning).await;
	let invoice = lightning.external.invoice_with_preimage(Some(sat(100_000)), "decided", "decided", [31; 32]).await;
	let hash = Preimage::from([31; 32]).compute_payment_hash();
	let (source, key) = htlc_send_source(&ctx, &srv, hash).await;
	let request = sign_lightning_send_request(&srv, send_request(invoice, &source), &key).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let refusal = decide_during(&db, &[source.id()], async move {
		rpc.initiate_lightning_payment(request).await
	}).await.unwrap_err();
	assert_eq!(refusal.metadata().get("abandon-reason").unwrap(), "decided_for_fallback");
	assert_eq!(attempt_count(&db, hash).await, 0);
}

/// F1: a claim on a grant decided while the claim was in flight is refused,
/// and its preimage is not stored.
#[tokio::test]
async fn claim_on_decided_grant_stores_no_preimage() {
	let ctx = TestContext::new("server/claim_on_decided_grant").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = payout_server(&ctx, &lightning).await;
	srv.wait_for_vtxopool(&ctx).await;
	let preimage = Preimage::random();
	let hash = preimage.compute_payment_hash();
	let receiver = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let mailbox = MailboxIdentifier::from_pubkey(receiver.public_key());
	let mut rpc = srv.get_public_rpc().await;
	let invoice = rpc.start_lightning_receive(protos::StartLightningReceiveRequest {
		payment_hash: hash.to_vec(), amount_sat: 100_000,
		min_cltv_delta: srv.config().max_user_invoice_cltv_delta.into(),
		mailbox_id: Some(mailbox.serialize()), description: None, expiry_secs: None,
		claim_pubkey: Some(receiver.public_key().serialize().to_vec()),
	}).await.unwrap().into_inner().bolt11;
	let payer = lightning.external;
	let payment = tokio::spawn(async move { payer.try_pay_bolt11(invoice).await });
	srv.wait_for_lightning_receive_accepted(hash).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let sub = db.read(async |t| t.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap();
	let expiry = sub.lowest_incoming_htlc_expiry.unwrap().saturating_sub(srv.config().htlc_expiry_delta);
	let grants = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: receiver.public_key().serialize().to_vec(),
		htlc_recv_expiry: expiry.into(), lightning_receive_anti_dos: None,
		claim_attestation: Some(LightningReceiveClaimAttestation::new(hash, expiry, &receiver).serialize()),
	}).await.unwrap().into_inner().htlc_vtxos.into_iter()
		.map(|raw| Vtxo::<Full>::deserialize(&raw).unwrap()).collect::<Vec<_>>();
	let keys = vec![receiver; grants.len()];
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		grants.clone(), VtxoPolicy::new_pubkey(receiver.public_key()),
	).unwrap().generate_user_nonces(&keys).unwrap();
	let mut package = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	let address = super::payout_address(&srv, VtxoPolicy::new_pubkey(receiver.public_key()), &receiver, mailbox).await;
	for output in package.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.blinded_mailbox_id = super::blinded_mailbox(&address);
	}
	let ids = grants.iter().map(|grant| grant.id()).collect::<Vec<_>>();
	let refusal = decide_during(&db, &ids, async move {
		rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
			payment_hash: hash.to_vec(), payment_preimage: preimage.to_vec(), cosign_request: Some(package),
		}).await
	}).await.unwrap_err();
	assert_eq!(refusal.metadata().get("abandon-reason").unwrap(), "decided_for_fallback");
	assert!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	payment.abort();
}

/// A3: the same signed send request sent again, while its payment is open
/// and after it succeeded, is acknowledged without a second attempt.
#[tokio::test]
async fn repeated_send_intent_is_acknowledged_once() {
	let ctx = TestContext::new("server/repeated_send_intent_acknowledged").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = payout_server(&ctx, &lightning).await;
	let preimage = Preimage::random();
	let hash = preimage.compute_payment_hash();
	let mut hold = lightning.external.hold_client().await;
	let invoice = hold.invoice(hold::InvoiceRequest {
		payment_hash: hash.to_vec(), amount_msat: 100_000_000,
		description: Some(hold::invoice_request::Description::Memo("repeated intent".into())),
		min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
	}).await.unwrap().into_inner().bolt11;
	let (source, key) = htlc_send_source(&ctx, &srv, hash).await;
	let request = sign_lightning_send_request(&srv, send_request(invoice, &source), &key).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let mut rpc = srv.get_public_rpc().await;
	lightning.sync().await;
	rpc.initiate_lightning_payment(request.clone()).await.unwrap();
	lightning.external.wait_for_hold_invoice_accepted(hash).await;
	rpc.initiate_lightning_payment(request.clone()).await.expect("a lost reply's retry is acknowledged");
	hold.settle(hold::SettleRequest { payment_preimage: preimage.to_vec() }).await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		while db.read(async |t| t.get_latest_payment_attempt_by_payment_hash(hash).await).await.unwrap()
			.unwrap().status != server::database::ln::LightningPaymentStatus::Succeeded {
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the payment did not succeed");
	rpc.initiate_lightning_payment(request).await.expect("a retry after success is acknowledged");
	assert_eq!(attempt_count(&db, hash).await, 1);
	assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM cln_xpay WHERE payment_hash=$1",
		&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 1);
}

/// A4: the server stops between canceling a receive and failing the intra-Ark
/// attempt that funded it. That attempt can never succeed, so its source is
/// refunded after its HTLC expiry rather than held.
#[tokio::test]
async fn open_attempt_on_canceled_receive_is_refunded_after_htlc_expiry() {
	let ctx = TestContext::new("server/open_attempt_on_canceled_receive").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.receive_htlc_forward_timeout = Duration::from_secs(2); })
		.create().await;
	let payer = ctx.bark_sdk("payer", &srv).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let receiver = ctx.bark_sdk("receiver", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	payer.stop_daemon_wait().await.unwrap();
	receiver.stop_daemon_wait().await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	db.write(async |t| {
		t.batch_execute("CREATE FUNCTION a4_refuse_failure() RETURNS trigger AS $$
			BEGIN RAISE EXCEPTION 'A4 stop before the attempt failure'; END; $$ LANGUAGE plpgsql;
			CREATE TRIGGER a4_refuse_failure BEFORE UPDATE ON lightning_payment_attempt
			FOR EACH ROW WHEN (NEW.status='failed') EXECUTE FUNCTION a4_refuse_failure();").await?;
		Ok(())
	}).await.unwrap();
	let invoice = receiver.bolt11_invoice(sat(10_000), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	Box::pin(payer.pay_lightning_invoice(invoice, None, false)).await.unwrap();
	tokio::time::timeout(Duration::from_secs(60), async {
		while db.read(async |t| t.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap().status
			!= server::database::ln::LightningHtlcSubscriptionStatus::Canceled {
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}).await.expect("the unclaimed receive was not canceled");
	let attempt = db.read(async |t| t.get_latest_payment_attempt_by_payment_hash(hash).await).await.unwrap().unwrap();
	assert!(!attempt.status.is_final(), "the attempt stays open on its canceled receive");
	let source_id: VtxoId = db.read(async |t| Ok(t.query_one("SELECT vtxo_id FROM lightning_payment_attempt_htlc_vtxo \
		WHERE lightning_payment_attempt_id=$1", &[&attempt.id]).await?.get::<_, String>(0))).await.unwrap().parse().unwrap();
	let source = payer.get_full_vtxo(source_id).await.unwrap();
	let htlc_expiry = source.policy().as_server_htlc_send().unwrap().htlc_expiry;
	let tip = ctx.bitcoind().get_block_count().await as u32;
	let tip = ctx.generate_blocks(htlc_expiry.to_u32() + 1 - tip).await;
	let refund = proposal(&db, &source, tip).await;
	assert_eq!(refund.disposition, birth_owner(&db, &source).await);
	assert_eq!(refund.evidence, Some(OwnershipEvidence::HtlcExternalFailed));
}

/// R6: a source never sent to a Lightning node is refunded after its HTLC
/// expiry even when a preimage for its hash is stored.
#[tokio::test]
async fn unattempted_source_with_stored_preimage_is_refunded() {
	let ctx = TestContext::new("server/unattempted_source_with_stored_preimage").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = payout_server(&ctx, &lightning).await;
	let preimage = Preimage::random();
	let hash = preimage.compute_payment_hash();
	let (source, _) = htlc_send_source(&ctx, &srv, hash).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	assert!(db.write(async |t| t.store_htlc_settlement(preimage).await).await.unwrap().is_some());
	let htlc_expiry = source.policy().as_server_htlc_send().unwrap().htlc_expiry;
	let tip = ctx.bitcoind().get_block_count().await as u32;
	let tip = ctx.generate_blocks(htlc_expiry.to_u32() + 1 - tip).await;
	let refund = proposal(&db, &source, tip).await;
	assert_eq!(refund.disposition, birth_owner(&db, &source).await);
	assert_eq!(refund.evidence, Some(OwnershipEvidence::HtlcUnattemptedExpired));
}
