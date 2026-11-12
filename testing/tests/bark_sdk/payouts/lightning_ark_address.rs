//! An invoice paid on its Ark address binds the coin to the receiver.

use ark::lightning::{Bolt11InvoiceExt, PaymentHash};
use ark_testing::{btc, sat, TestContext};
use bark::actions::lightning::receive::LightningReceiveState;
use server::database::Db;

#[tokio::test]
async fn ark_address_payment_binds_the_receivers_mailbox() {
	let ctx = TestContext::new("bark_sdk/payouts/ark_address_payment_binds_the_receivers_mailbox").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| {
			cfg.expiry_payouts = true;
			cfg.ln_receive_ark_address_hints = true;
		})
		.create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let payer = ctx.bark_sdk("payer", &srv).boarded(sat(400_000))
		.cfg(|cfg| {
			cfg.daemon_manual_sync = true;
			cfg.lightning_ark_address_hints = true;
		})
		.create().await;
	let receiver = ctx.bark_sdk("receiver", &srv)
		.cfg(|cfg| {
			cfg.daemon_manual_sync = true;
			cfg.lightning_ark_address_hints = true;
		})
		.create().await;

	let amount = sat(50_000);
	let invoice = receiver.bolt11_invoice(amount, None, None).await.unwrap();
	assert!(invoice.ark_address().is_some());
	let hash = PaymentHash::from(&invoice);

	payer.pay_lightning_invoice(invoice, None, true).await.unwrap();
	receiver.sync().await;
	assert!(matches!(receiver.lightning_receive_state(hash).await.unwrap(),
		LightningReceiveState::Settled(_)));

	// Its expiry payout goes to the receiver's mailbox.
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let coins = receiver.spendable_vtxos().await.unwrap();
	assert!(!coins.is_empty());
	let mailbox = receiver.mailbox_identifier().as_pubkey().to_string();
	for coin in coins {
		let id = coin.id().to_string();
		let owner = db.read(async |tx| Ok(
			tx.query_one("SELECT mailbox_pk FROM coin_owner WHERE vtxo_id = $1", &[&id]).await?
				.get::<_, String>(0)
		)).await.unwrap();
		assert_eq!(owner, mailbox);
	}
}
