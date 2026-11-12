use bitcoin::secp256k1::{Keypair, rand::thread_rng};
use ark::{ProtocolEncoding, ServerVtxo, ServerVtxoPolicy, VtxoPolicy, SECP};
use ark::mailbox::MailboxIdentifier;
use ark::test_util::dummy::DummyTestVtxoSpec;
use ark::vtxo::raw::RawVtxo;
use server::database::Db;
use server_rpc::protos;
use ark_testing::TestContext;
use server::database::tree::VtxoTreeUpdate;

/// The arkoor post endpoint is unauthenticated (senders aren't the
/// recipient), so the server must only accept vtxos it cosigned itself.
/// Unknown ids and posts whose content claims a pubkey different from the
/// server's records are rejected; without this anyone could grow the
/// mailbox table with arbitrary blobs.
#[tokio::test]
async fn payout_mailbox_post_requires_known_vtxo_and_its_owner() {
	let ctx = TestContext::new("server/payout_mailbox_post_requires_known_vtxo_and_its_owner").await;
	let srv = ctx.captaind("server").cfg(|config| config.expiry_payouts = true).create().await;

	let db = Db::connect(&srv.config().postgres).await.expect("connect to captaind's postgres");
	let mut rpc = srv.get_mailbox_public_rpc().await;

	let mailbox_kp = Keypair::new(&SECP, &mut thread_rng());
	let mailbox_id = MailboxIdentifier::from_pubkey(mailbox_kp.public_key());
	let mailbox_pubkey = srv.ark_info().await.mailbox_pubkey;

	// Seed one vtxo into the vtxo table, as if the server cosigned it.
	let owner_kp = Keypair::new(&SECP, &mut thread_rng());
	let (_tx, vtxo) = DummyTestVtxoSpec {
		user_keypair: owner_kp,
		..Default::default()
	}.build();
	db.write(async |t| t.execute_vtxo_tree_update(VtxoTreeUpdate::new()
		.insert_spendable_vtxos([ServerVtxo::from(vtxo.clone())])
		.bind_owner_mailboxes([(vtxo.id(), mailbox_kp.public_key())])?).await).await
		.expect("upsert vtxo");

	// A vtxo the server never cosigned is rejected.
	let attacker_kp = Keypair::new(&SECP, &mut thread_rng());
	let (_tx, unknown_vtxo) = DummyTestVtxoSpec {
		user_keypair: attacker_kp,
		..Default::default()
	}.build();
	let err = rpc.post_arkoor_message(protos::mailbox_server::PostArkoorMessageRequest {
		blinded_id: mailbox_id.to_blinded(mailbox_pubkey, &attacker_kp).unwrap().as_ref().to_vec(),
		vtxos: vec![ProtocolEncoding::serialize(&unknown_vtxo).to_vec()],
	}).await.unwrap_err();
	assert!(err.message().contains("does not exist"),
		"unexpected error for unknown vtxo: {}", err.message(),
	);

	// A known id whose content claims a different pubkey is rejected; it
	// would route the vtxo into a mailbox its owner doesn't watch.
	let mut raw = RawVtxo::deserialize(&ProtocolEncoding::serialize(&vtxo)).unwrap();
	raw.policy = ServerVtxoPolicy::User(VtxoPolicy::new_pubkey(attacker_kp.public_key()));
	let err = rpc.post_arkoor_message(protos::mailbox_server::PostArkoorMessageRequest {
		blinded_id: mailbox_id.to_blinded(mailbox_pubkey, &attacker_kp).unwrap().as_ref().to_vec(),
		vtxos: vec![raw.serialize()],
	}).await.unwrap_err();
	assert!(err.message().contains("doesn't belong to the provided vtxo pubkey"),
		"unexpected error for pubkey mismatch: {}", err.message(),
	);

	// Equal payout scripts do not make opposite mailbox points the same recipient.
	let other_mailbox = MailboxIdentifier::from_pubkey(mailbox_id.as_pubkey().negate(&SECP));
	assert_ne!(other_mailbox, mailbox_id);
	assert_eq!(ark::fallback::payout_script(other_mailbox.as_pubkey()),
		ark::fallback::payout_script(mailbox_id.as_pubkey()));
	let err = rpc.post_arkoor_message(protos::mailbox_server::PostArkoorMessageRequest {
		blinded_id: other_mailbox.to_blinded(mailbox_pubkey, &owner_kp).unwrap().as_ref().to_vec(),
		vtxos: vec![ProtocolEncoding::serialize(&vtxo).to_vec()],
	}).await.unwrap_err();
	assert_eq!(err.code(), tonic::Code::InvalidArgument);
	let entries = db.read(async |t| t.get_mailbox_entries(other_mailbox, 0, 100).await).await.unwrap();
	assert!(entries.is_empty(), "a post cannot consume another recipient's delivery slot");

	// Nothing landed in the mailbox.
	let entries = db.read(async |t| t.get_mailbox_entries(mailbox_id, 0, 100).await).await.unwrap();
	assert!(entries.is_empty(), "rejected posts should not create mailbox entries");

	// The genuine vtxo still goes through.
	rpc.post_arkoor_message(protos::mailbox_server::PostArkoorMessageRequest {
		blinded_id: mailbox_id.to_blinded(mailbox_pubkey, &owner_kp).unwrap().as_ref().to_vec(),
		vtxos: vec![ProtocolEncoding::serialize(&vtxo).to_vec()],
	}).await.expect("post of a known vtxo should succeed");

	let entries = db.read(async |t| t.get_mailbox_entries(mailbox_id, 0, 100).await).await.unwrap();
	assert_eq!(entries.len(), 1, "the genuine vtxo should be delivered");
}
