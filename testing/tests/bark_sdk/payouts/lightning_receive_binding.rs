//! Successful receives deliver the actual recipient's signed value once.

use std::time::Duration;
use ark::attestations::LightningReceiveClaimAttestation;
use ark::lightning::PaymentHash;
use ark::{ProtocolEncoding, SECP};
use ark_testing::{btc, sat, TestContext};
use bark::actions::lightning::receive::LightningReceiveState;
use bitcoin::secp256k1::Keypair;
use server::database::Db;
use server_rpc::protos;

/// A wallet receiving for itself: the grant key, the claim key and the payee
/// are one key, and no other key can take the grant.
#[tokio::test]
async fn receive_binding_self_receive_is_bound_to_the_wallet_key() {
	let ctx = TestContext::new("bark_sdk/receive_binding_self_receive").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10)).cfg(|cfg| cfg.expiry_payouts = true).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let wallet = ctx.bark_sdk("wallet", &srv).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let invoice = wallet.bolt11_invoice(sat(50_000), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let sub = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap();
	let claim_key = sub.claim_pubkey.expect("the wallet binds its receive to a claim key");
	let mut rpc = srv.get_public_rpc().await;
	lightning.sync().await;
	let (paid, claimed) = tokio::time::timeout(Duration::from_secs(60), async {
		tokio::join!(
			lightning.external.try_pay_bolt11(invoice.to_string()),
			async {
				let lowest = loop {
					let sub = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await)
						.await.unwrap().unwrap();
					if let Some(lowest) = sub.lowest_incoming_htlc_expiry { break lowest; }
					tokio::time::sleep(Duration::from_millis(50)).await;
				};
				let other = Keypair::from_seckey_slice(&SECP, &[75; 32]).unwrap();
				let expiry = lowest.saturating_sub(srv.config().htlc_expiry_delta);
				let refused = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
					payment_hash: hash.to_vec(),
					user_pubkey: other.public_key().serialize().to_vec(),
					htlc_recv_expiry: expiry.into(),
					lightning_receive_anti_dos: None,
					claim_attestation: Some(LightningReceiveClaimAttestation::new(hash, expiry, &other).serialize()),
				}).await.expect_err("another key won the receive grant");
				assert_eq!(refused.code(), tonic::Code::InvalidArgument);
				assert!(refused.message().contains("claim key"), "{refused}");
				wallet.try_claim_lightning_receive(hash, true).await
			},
		)
	}).await.expect("self receive did not complete");
	paid.expect("external payment failed");
	assert!(matches!(claimed.unwrap(), LightningReceiveState::Settled(_)));
	// The grant was locked to the claim key, and the claim, which the payout
	// engine reads, pays that same key.
	let claims = db.read(async |tx| Ok(tx.query("SELECT c.output_vtxo, s.vtxo FROM expiry_lightning_receive_claim c \
		JOIN vtxo s ON s.vtxo_id=c.source_vtxo_id WHERE c.payment_hash=$1", &[&hash.to_string()]).await?)).await.unwrap();
	assert!(!claims.is_empty());
	for claim in claims {
		let grant = ark::Vtxo::<ark::vtxo::Full>::deserialize(claim.get("vtxo")).unwrap();
		let output = ark::Vtxo::<ark::vtxo::Full>::deserialize(claim.get("output_vtxo")).unwrap();
		assert_eq!(grant.user_pubkey(), claim_key);
		assert_eq!(output.user_pubkey(), claim_key);
	}
	wallet.sync().await;
	assert!(wallet.spendable_vtxos().await.unwrap().iter().any(|coin| coin.user_pubkey() == claim_key));
}
