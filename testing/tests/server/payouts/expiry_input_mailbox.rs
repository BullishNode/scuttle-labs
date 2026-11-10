use ark::arkoor::package::ArkoorPackageBuilder;
use ark::mailbox::MailboxIdentifier;
use ark::{ProtocolEncoding, VtxoPolicy, SECP};
use ark_testing::{btc, sat, TestContext};
use bitcoin::secp256k1::Keypair;
use server::database::{Db, SpendState};
use server_rpc::protos;

/// Every input in a multi-part OOR request needs the same proven funding
/// mailbox before the server cosigns. A refused mixed request must not leave
/// a destination binding or consume either input; a same-M request must pass.
#[tokio::test]
async fn mixed_input_mailboxes_are_refused_before_cosign() {
	let ctx = TestContext::new("server/mixed_input_mailboxes_before_cosign").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool()
		.cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let owner = ctx.bark("owner", &srv).funded(sat(600_000)).create().await;
	let first_id = owner.board_and_confirm_and_register(&ctx, sat(100_000)).await[0];
	let second_id = owner.board_and_confirm_and_register(&ctx, sat(100_000)).await[0];
	let other = ctx.bark("other", &srv).funded(sat(300_000)).create().await;
	let other_id = other.board_and_confirm_and_register(&ctx, sat(100_000)).await[0];
	let owner_wallet = owner.client().await;
	let other_wallet = other.client().await;
	assert_ne!(owner_wallet.mailbox_identifier(), other_wallet.mailbox_identifier());
	let first = owner_wallet.get_full_vtxo(first_id).await.unwrap();
	let second = owner_wallet.get_full_vtxo(second_id).await.unwrap();
	let foreign = other_wallet.get_full_vtxo(other_id).await.unwrap();
	let first_key = owner_wallet.get_vtxo_key(&first).await.unwrap();
	let second_key = owner_wallet.get_vtxo_key(&second).await.unwrap();
	let foreign_key = other_wallet.get_vtxo_key(&foreign).await.unwrap();
	let recipient = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let mailbox = MailboxIdentifier::from_pubkey(Keypair::new(&SECP, &mut bip39::rand::thread_rng()).public_key());
	let policy = VtxoPolicy::new_pubkey(recipient.public_key());
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let mut rpc = srv.get_public_rpc().await;

	let mixed = ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		[first.clone(), foreign], policy.clone()).unwrap()
		.generate_user_nonces(&[first_key, foreign_key]).unwrap();
	let mixed_ids = mixed.build_unsigned_vtxos().map(|v| v.id()).collect::<Vec<_>>();
	let mut request = protos::ArkoorPackageCosignRequest::from(mixed.cosign_request());
	for output in request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.mailbox_id = Some(mailbox.serialize());
	}
	let refusal = rpc.request_arkoor_cosign(request).await.expect_err("mixed input mailboxes must fail before cosign");
	assert_eq!(refusal.code(), tonic::Code::InvalidArgument, "{refusal}");
	assert!(refusal.message().contains("mixed payout mailboxes"), "{refusal}");
	assert_eq!(db.read(async |tx| tx.coin_owner_mailboxes(&mixed_ids).await).await.unwrap().len(), 0);
	assert!(db.read(async |tx| tx.get_existing_server_vtxos_by_id(&mixed_ids).await).await.unwrap().is_empty());
	for id in [first_id, other_id] {
		assert_eq!(db.read(async |tx| tx.get_user_vtxo_by_id(id).await).await.unwrap().spend_state,
			SpendState::Spendable);
	}

	let accepted = ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		[first, second], policy).unwrap().generate_user_nonces(&[first_key, second_key]).unwrap();
	let accepted_ids = accepted.build_unsigned_vtxos().map(|v| v.id()).collect::<Vec<_>>();
	let mut request = protos::ArkoorPackageCosignRequest::from(accepted.cosign_request());
	for output in request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.mailbox_id = Some(mailbox.serialize());
	}
	rpc.request_arkoor_cosign(request).await.expect("same-M inputs should be cosigned");
	let bindings = db.read(async |tx| tx.coin_owner_mailboxes(&accepted_ids).await).await.unwrap();
	assert_eq!(bindings.len(), accepted_ids.len());
	assert!(accepted_ids.iter().all(|id| bindings.get(id) == Some(&mailbox.as_pubkey())));
}
