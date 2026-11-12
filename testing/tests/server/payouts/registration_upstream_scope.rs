// Payout tests that use helpers private to the upstream `registration` test module.

#[tokio::test]
async fn payout_unregistered_arkoor_does_not_credit_recipient() {
	unregistered_arkoor_recipient(true).await;
}

async fn unregistered_arkoor_recipient(payout: bool) {
	let ctx = TestContext::new(if payout {
		"server/payout_unregistered_arkoor_does_not_credit_recipient"
	} else { "server/unregistered_arkoor_does_not_credit_recipient" }).await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool().watchmand()
		.cfg(move |cfg| cfg.expiry_payouts = payout).create().await;
	let sender = ctx.bark("sender", &srv).funded(sat(500_000)).create().await;
	sender.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	ctx.refresh_all(&srv, &[&sender]).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	sender.sync().await;
	let stale = sender.full_clone("stale_sender").await;
	let receiver = ctx.bark("receiver", &srv).create().await;
	let proxy = srv.start_proxy_no_mailbox(DropRegistration).await;
	sender.set_ark_url(&proxy.address).await;
	sender.send_oor(receiver.address().await, sat(50_000)).await;
	stale.start_exit_all().await;
	complete_exit(&ctx, &stale).await;
	stale.claim_all_exits(stale.get_onchain_address().await).await;
	let tip = ctx.generate_blocks(1).await;
	srv.watchmand().wait_for_sync_height(tip).await;
	receiver.sync().await;
	let child_ids = receiver.vtxo_ids_no_sync().await;
	assert_eq!(child_ids.len(), 1);
	let child = receiver.raw_vtxo(child_ids[0]).await;
	assert!(child.has_all_witnesses(), "the wallet must retain the signed chain");
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let server_state = db.read(async |t| t.get_user_vtxo_by_id(child.id()).await)
		.await.unwrap().spend_state;
	assert_eq!(server_state, server::database::SpendState::Unregistered);
	assert_eq!(receiver.spendable_balance_no_sync().await, Amount::ZERO);
	assert!(receiver.history().await.is_empty());

	let honest = ctx.bark("honest", &srv).funded(sat(100_000)).create().await;
	honest.board_and_confirm_and_register(&ctx, sat(80_000)).await;
	honest.send_oor(receiver.address().await, sat(20_000)).await;
	receiver.sync().await;
	assert_eq!(receiver.spendable_balance_no_sync().await, sat(20_000));
	assert_eq!(receiver.history().await.len(), 1);
}
