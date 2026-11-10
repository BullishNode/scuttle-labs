use ark::{ProtocolEncoding, VtxoRequest};
use ark::mailbox::MailboxIdentifier;
use ark::tree::signed::{UnlockHash, UnlockPreimage};
use bitcoin::secp256k1::Keypair;
use server::database::rounds::StoredRoundOutput;

use super::*;

/// Identical policies and amounts cannot identify which mailbox belongs to
/// which output. The stored request position must survive a physical row
/// reorder, and a missing position must stop round completion.
#[tokio::test]
async fn stored_round_outputs_keep_mailboxes_by_position() {
	let mut ctx = TestContext::new_minimal("postgresd/round_output_mailbox_position").await;
	ctx.init_central_postgres().await;
	let postgres = ctx.new_postgres(&ctx.test_name).await;
	Db::create(&postgres).await.unwrap();
	let db = Db::connect(&postgres).await.unwrap();
	let input = ServerVtxo::from(VTXO_VECTORS.board_vtxo.clone());
	db.write(async |tx| tx.upsert_vtxos(&[input.clone()]).await).await.unwrap();

	let first = MailboxIdentifier::from_pubkey(Keypair::from_seckey_slice(&ark::SECP, &[17; 32])
		.unwrap().public_key());
	let second = MailboxIdentifier::from_pubkey(Keypair::from_seckey_slice(&ark::SECP, &[18; 32])
		.unwrap().public_key());
	let policy = VtxoPolicy::new_pubkey(VTXO_VECTORS.board_vtxo.user_pubkey());
	let outputs = [first, second].map(|mailbox| StoredRoundOutput {
		vtxo_request: VtxoRequest { policy: policy.clone(), amount: sat(1_000) },
		unblinded_mailbox_id: Some(mailbox),
	});
	let preimage: UnlockPreimage = [71u8; 32];
	let hash = UnlockHash::hash(&preimage);
	db.write(async |tx| tx.try_store_round_participation(BlockHeight::new(0), preimage,
		&[input.id()], outputs.iter(), None).await).await.unwrap();
	let part_id = db.read(async |tx| Ok(tx.query_one(
		"SELECT id FROM round_participation WHERE unlock_hash=$1", &[&hash.to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();

	// Reinsert the same two rows in reverse physical order. A policy/amount
	// comparison alone cannot detect a swapped mailbox here.
	db.write(async |tx| {
		tx.execute("DELETE FROM round_part_output WHERE participation_id=$1", &[&part_id]).await?;
		for (index, mailbox) in [(1i32, second), (0i32, first)] {
			tx.execute("INSERT INTO round_part_output \
				(participation_id,policy,amount,unblinded_mailbox_id,output_index) \
				VALUES($1,$2,$3,$4,$5)",
				&[&part_id, &policy.serialize(), &1_000i64, &mailbox.to_string(), &index]).await?;
		}
		Ok(())
	}).await.unwrap();
	let stored = db.read(async |tx| tx.get_round_participation_by_unlock_hash(hash).await)
		.await.unwrap().unwrap();
	assert_eq!(stored.outputs.len(), 2);
	assert_eq!(stored.outputs[0].unblinded_mailbox_id, Some(first));
	assert_eq!(stored.outputs[1].unblinded_mailbox_id, Some(second));
	assert!(stored.outputs.iter().all(|output| output.vtxo_request.policy == policy
		&& output.vtxo_request.amount == sat(1_000)));

	db.write(async |tx| {
		tx.execute("UPDATE round_part_output SET output_index=NULL \
			WHERE participation_id=$1 AND output_index=0", &[&part_id]).await?;
		Ok(())
	}).await.unwrap();
	let err = db.read(async |tx| tx.get_round_participation_by_unlock_hash(hash).await)
		.await.expect_err("a missing stored position must hold the round");
	assert!(format!("{err:#}").contains("round output order is missing or ambiguous"), "{err:#}");
}
