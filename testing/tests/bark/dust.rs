use bitcoin_ext::P2TR_DUST_SAT;

use ark_testing::{btc, sat, TestContext};

#[tokio::test]
async fn dust_arkoor_is_rejected_before_cosign() {
	let ctx = TestContext::new("bark/dust_arkoor_is_rejected_before_cosign").await;
	let srv = ctx.captaind("server").funded(btc(10)).create().await;
	let bark1 = ctx.bark("bark1", &srv).funded(sat(1_000_000)).create().await;
	let bark2 = ctx.bark("bark2", &srv).funded(sat(1_000_000)).create().await;

	let board_amount = sat(800_000);
	bark1.board_and_confirm_and_register(&ctx, board_amount).await;

	let dust_amount = sat(P2TR_DUST_SAT - 1);
	let err = bark1.try_send_oor(&bark2.address().await, dust_amount, true).await.unwrap_err();
	let err_str = format!("{err:?}");
	assert!(err_str.contains("cannot be enforced onchain"), "unexpected error: {err_str}");
	assert_eq!(bark1.spendable_balance().await, board_amount);
	bark2.sync().await;
	assert_eq!(bark2.spendable_balance().await, sat(0));

	// A small, relayable payment still works through the same path.
	bark1.send_oor(&bark2.address().await, sat(500)).await;
	bark2.sync().await;
	assert_eq!(bark2.spendable_balance().await, sat(500));
}

/// A 1,316-sat source split four ways produced the nonrelayable checkpoints
/// used in the batched double-receipt proof. Refuse it before cosigning.
#[tokio::test]
async fn four_way_dust_checkpoint_is_rejected() {
	let ctx = TestContext::new("bark/four_way_dust_checkpoint_is_rejected").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool()
		.cfg(|cfg| cfg.min_board_amount = sat(0)).create().await;
	let sender = ctx.bark("sender", &srv).funded(sat(100_000)).cfg(|cfg| {
		cfg.change_vtxo_split_factor = 3;
	}).create().await;
	let receiver = ctx.bark("receiver", &srv).create().await;
	sender.board_and_confirm_and_register(&ctx, sat(1_316)).await;
	let err = sender.try_send_oor(receiver.address().await, sat(329), true).await.unwrap_err();
	assert!(format!("{err:#}").contains("cannot be enforced onchain"), "unexpected error: {err:#}");
	sender.assert_unchanged_after_refusal(sat(1_316)).await;
	assert_eq!(receiver.spendable_balance().await, sat(0));
}
