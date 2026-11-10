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

#[tokio::test]
async fn receive_binding_normal_external_claim_keeps_the_actual_recipient() {
	let ctx = TestContext::new("bark_sdk/receive_binding_normal_external_claim").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10)).cfg(|cfg| cfg.expiry_payouts = true).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let provider = ctx.bark_sdk("provider", &srv).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let address = recipient.new_address().await.unwrap();
	let recipient_key = address.policy().user_pubkey();
	assert_ne!(provider.mailbox_identifier(), recipient.mailbox_identifier());
	let server_pubkey = srv.ark_info().await.server_pubkey;
	let other = Keypair::from_seckey_slice(&SECP, &[73; 32]).unwrap();
	let wrong_server_address = ark::Address::new(true, other.public_key(),
		address.policy().clone(), address.delivery().to_vec());
	let amount = sat(50_000);
	assert!(provider.bolt11_invoice_for_address(amount, wrong_server_address, None, None).await.is_err(),
		"the wallet must reject an address for another server before issuing an invoice");
	assert_ne!(server_pubkey, other.public_key());
	let invoice = provider.bolt11_invoice_for_address(amount, address.clone(), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let sub = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap();
	assert!(sub.claim_pubkey.is_some(), "SDK receive must bind its claim key");
	assert_eq!(db.read(async |tx| tx.get_lightning_receiver_mailbox_id(hash).await).await.unwrap(),
		Some(provider.mailbox_identifier()), "the grant belongs to the provider until a final claim");
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_lightning_receive_claim WHERE payment_hash=$1",
		&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
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
				let attacker = Keypair::from_seckey_slice(&SECP, &[74; 32]).unwrap();
				let expiry = lowest.saturating_sub(srv.config().htlc_expiry_delta);
				let refused = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
					payment_hash: hash.to_vec(),
					user_pubkey: attacker.public_key().serialize().to_vec(),
					htlc_recv_expiry: expiry.into(),
					lightning_receive_anti_dos: None,
					claim_attestation: Some(LightningReceiveClaimAttestation::new(hash, expiry, &attacker).serialize()),
				}).await.expect_err("a foreign key won the receive grant");
				assert_eq!(refused.code(), tonic::Code::InvalidArgument);
				assert!(refused.message().contains("claim key"), "{refused}");
				provider.try_claim_lightning_receive(hash, true).await
			},
		)
	}).await.expect("normal receive did not complete");
	paid.expect("external payment failed");
	assert!(matches!(claimed.unwrap(), LightningReceiveState::Settled(_)));
	let claims = db.read(async |tx| Ok(tx.query("SELECT source_vtxo_id,output_vtxo,output_mailbox \
		FROM expiry_lightning_receive_claim WHERE payment_hash=$1", &[&hash.to_string()]).await?)).await.unwrap();
	assert!(!claims.is_empty(), "the completed claim must bind its actual outputs");
	for claim in &claims {
		let output = ark::Vtxo::<ark::vtxo::Full>::deserialize(claim.get("output_vtxo")).unwrap();
		assert_eq!(output.user_pubkey(), recipient_key);
		assert_eq!(claim.get::<_, &str>("output_mailbox"), recipient.mailbox_identifier().as_pubkey().to_string());
		let provider_owner = db.read(async |tx| Ok(tx.query_one(
			"SELECT mailbox_pk FROM coin_owner WHERE vtxo_id=$1",
			&[&claim.get::<_, &str>("source_vtxo_id")]).await?.get::<_, String>(0))).await.unwrap();
		assert_eq!(provider_owner, provider.mailbox_identifier().as_pubkey().to_string());
	}
	recipient.sync().await;
	let coins = recipient.spendable_vtxos().await.unwrap();
	assert!(!coins.is_empty(), "normal delivery must give the recipient its signed coins");
	let received = coins.iter().map(|coin| coin.amount()).sum::<bitcoin::Amount>();
	assert_eq!(received, sat(50_000));
	for coin in &coins {
		assert_eq!(coin.user_pubkey(), recipient_key);
		let full = recipient.get_full_vtxo(coin.id()).await.unwrap();
		let funding = ctx.bitcoind().await_transaction(full.chain_anchor().txid).await;
		full.validate(&funding).unwrap();
	}
	assert!(matches!(provider.try_claim_lightning_receive(hash, true).await.unwrap(), LightningReceiveState::Settled(_)));
	recipient.sync().await;
	assert_eq!(recipient.spendable_vtxos().await.unwrap().iter().map(|coin| coin.amount()).sum::<bitcoin::Amount>(), received,
		"an identical claim retry cannot pay the recipient twice");
}

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
