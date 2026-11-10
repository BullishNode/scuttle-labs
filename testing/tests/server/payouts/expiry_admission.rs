pub(super) use super::owner_helpers::{owner_rows, source_rows, vtxo_replay_rows, receive_admission_rows};

use ark::arkoor::package::ArkoorPackageBuilder;
use ark::lightning::PaymentHash;
use ark::{ProtocolEncoding, VtxoPolicy};
use ark::address::VtxoDelivery;
use ark::mailbox::{MailboxIdentifier, MailboxType};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::Keypair;
use ark_testing::daemon::captaind::Captaind;
use server::database::Db;
use server_rpc::protos;
use bitcoin_ext::{BlockDelta, BlockHeight};

use ark_testing::{btc, sat, TestContext};

/// Both early request rejection and the final writer fence must carry the same
/// stable reason. The durable decision here is an isolated fixture;
/// allocation-operation races have separate coverage.
#[tokio::test]
async fn expiry_native_rpc_refusals_have_stable_reasons() {
	let ctx = TestContext::new("server/expiry_typed_refusals").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); }).create().await;
	let sender = ctx.bark("sender", &srv).funded(sat(500_000)).create().await;
	let ids = sender.board_and_confirm_and_register(&ctx,sat(100_000)).await;
	let wallet = sender.client().await;
	let source = wallet.get_full_vtxo(ids[0]).await.unwrap();
	let key = wallet.get_vtxo_key(&source).await.unwrap();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let expired = ctx.generate_blocks(source.expiry_height().to_u32()+1-tip.to_u32()).await;
	srv.wait_for_sync_height(expired).await;
	let request = async || {
		let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([source.clone()],
			VtxoPolicy::new_server_htlc_send(key.public_key(),
				PaymentHash::from(sha256::Hash::hash(b"typed expiry refusal").to_byte_array()),
				expired+srv.config().htlc_send_expiry_delta+BlockDelta::new(1)),
		).unwrap().generate_user_nonces(&[key]).unwrap();
		protos::LightningPayHtlcCosignRequest {
			parts: super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
		}
	};
	let mut rpc = srv.get_public_rpc().await;
	let error = rpc.request_lightning_pay_htlc_cosign(request().await).await.expect_err("new obligation on expired backing");
	assert_eq!(error.metadata().get("abandon-reason").unwrap().to_str().unwrap(), "expired_new_lightning");
	let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
	db.write(async |t| {
		t.query_one("INSERT INTO expiry_entitlement(vtxo_id) VALUES($1) RETURNING id",
			&[&source.id().to_string()]).await?;
		t.execute("UPDATE vtxo SET spend_state='spent',updated_at=NOW() WHERE vtxo_id=$1", &[&source.id().to_string()]).await?;
		Ok(())
	}).await.unwrap();
	let (full,bare,policy) = db.read(async |t| Ok((
		t.get_user_vtxo_by_id(source.id()).await?,
		t.get_bare_vtxo_by_id(source.id()).await?,
		t.get_vtxos_by_policy_page(source.policy(), None, 10).await?,
	))).await.unwrap();
	assert!(full.decided_for_fallback && bare.decided_for_fallback);
	assert!(policy.iter().find(|v| v.vtxo_id==source.id()).unwrap().decided_for_fallback);
	let error = rpc.request_lightning_pay_htlc_cosign(request().await).await.expect_err("already-decided source must have a specific refusal");
	assert_eq!(error.metadata().get("abandon-reason").unwrap().to_str().unwrap(), "decided_for_fallback");
	assert_eq!(full.vtxo.amount(),sat(100_000));
}

/// Admission binds the actual output VTXO to the submitted address. A failed
/// request writes nothing, and a replay cannot replace the first mailbox.
#[tokio::test]
async fn arkoor_output_destination_is_atomic_and_immutable() {
	let ctx = TestContext::new("server/arkoor_output_destination_binding").await;
	let srv = ctx.captaind("server").no_vtxo_pool()
		.cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let wallet = ctx.bark_sdk("sender", &srv).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let input = wallet.spendable_vtxos().await.unwrap().pop().unwrap();
	let input = wallet.get_full_vtxo(input.id()).await.unwrap();
	let input_key = wallet.get_vtxo_key(&input).await.unwrap();
	let recipient = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let first_mailbox = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let other_mailbox = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let server_pubkey = srv.ark_info().await.server_pubkey;
	let policy = VtxoPolicy::new_pubkey(recipient.public_key());
	let address = |mailbox: &Keypair| {
		let blinded_id = MailboxIdentifier::from_pubkey(mailbox.public_key())
			.to_blinded(srv.mailbox_keypair().public_key(), &recipient).unwrap();
		ark::Address::new(true, server_pubkey, policy.clone(),
			vec![VtxoDelivery::ServerMailbox { blinded_id }]).to_string()
	};
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input.clone()], policy.clone())
		.unwrap().generate_user_nonces(&[input_key]).unwrap();
	let output_id = builder.build_unsigned_vtxos().next().unwrap().id();
	let mut request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	let output = request.parts[0].outputs.first_mut().unwrap();
	output.address = Some(address(&first_mailbox));
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let original_source = source_rows(&db, &[input.id(), output_id]).await;
	let original_owners = owner_rows(&db).await;
	let mut rpc = srv.get_public_rpc().await;

	let mut missing = request.clone();
	missing.parts[0].outputs[0].address = None;
	let error = rpc.request_arkoor_cosign(missing).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error}");
	assert_eq!(source_rows(&db, &[input.id(), output_id]).await, original_source);
	assert_eq!(owner_rows(&db).await, original_owners);

	let mut wrong_key = request.clone();
	wrong_key.parts[0].outputs[0].policy =
		VtxoPolicy::new_pubkey(other_mailbox.public_key()).serialize();
	let error = rpc.request_arkoor_cosign(wrong_key).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument, "{error}");
	assert_eq!(source_rows(&db, &[input.id(), output_id]).await, original_source);
	assert_eq!(owner_rows(&db).await, original_owners);

	rpc.request_arkoor_cosign(request.clone()).await.unwrap();
	let bound = db.read(async |tx| {
		let row = tx.query_one("SELECT mailbox_pk FROM coin_owner WHERE vtxo_id=$1",
			&[&output_id.to_string()]).await?;
		Ok(row.get::<_, String>(0))
	}).await.unwrap();
	assert_eq!(bound, first_mailbox.public_key().to_string());
	let committed_source = source_rows(&db, &[input.id(), output_id]).await;
	let committed_owners = owner_rows(&db).await;
	let wrong_mailbox = MailboxIdentifier::from_pubkey(other_mailbox.public_key());
	assert!(db.write(async |tx| tx.store_vtxo_ids_in_mailbox(
		MailboxType::RecoveryVtxoId, wrong_mailbox, &[output_id],
	).await).await.is_err(), "a recovery hint cannot relabel a committed coin");
	assert_eq!(owner_rows(&db).await, committed_owners);

	let mut changed = request.clone();
	changed.parts[0].outputs[0].address = Some(address(&other_mailbox));
	assert!(rpc.request_arkoor_cosign(changed).await.is_err());
	assert_eq!(source_rows(&db, &[input.id(), output_id]).await, committed_source);
	assert_eq!(owner_rows(&db).await, committed_owners);

	// Losing the first reply and retrying with the same destination is safe.
	rpc.request_arkoor_cosign(request).await.unwrap();
	let after = source_rows(&db, &[input.id(), output_id]).await;
	assert_eq!(vtxo_replay_rows(after.iter().map(|s| serde_json::from_str(s).unwrap()).collect()),
		vtxo_replay_rows(committed_source.iter().map(|s| serde_json::from_str(s).unwrap()).collect()));
	assert_eq!(owner_rows(&db).await, committed_owners);
}
