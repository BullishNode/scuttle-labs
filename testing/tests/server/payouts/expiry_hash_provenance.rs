use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::{PaymentHash, Preimage};
use ark::VtxoPolicy;
use ark_testing::{btc, sat, TestContext};
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use server::database::expiry_ownership::OwnershipDisposition;
use server::database::Db;
use server_rpc::protos;

/// Two grants share a hash, but only one funds the paid attempt.
/// The other grant returns to its owner after expiry when the recorded success
/// accounts for the invoice and no attempt using that grant remains open.
#[tokio::test]
async fn expiry_unused_same_hash_grant_returns_to_owner_after_other_source_paid() {
	let name = "server/expiry_unused_same_hash_grant_owner_payout";
	let ctx = TestContext::new(name).await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).no_vtxo_pool().funded(btc(10))
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(|cfg| cfg.watchman.sweep_interval = Duration::from_secs(1)).create().await;
	let wallet = ctx.bark("owner", &srv).funded(sat(500_000)).create().await;
	let first = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await[0];
	let second = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await[0];
	let client = wallet.client().await;
	let preimage = Preimage::from([23; 32]);
	let hash = preimage.compute_payment_hash();
	let invoice = lightning.external.invoice_with_preimage(Some(sat(90_000)), "one-of-two-grants", "Exactly one source attempted", [23; 32]).await;
	let parsed: bark::lightning_invoice::Bolt11Invoice = invoice.parse().unwrap();
	assert_eq!(PaymentHash::from(&parsed), hash);
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let mut rpc = srv.get_public_rpc().await;
	let mut grants = Vec::new();
	let mut signed_grants = Vec::new();
	let mut sources = Vec::new();
	for id in [first, second] {
		let input = client.get_full_vtxo(id).await.unwrap();
		let key = client.get_vtxo_key(&input).await.unwrap();
		let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input.clone()],
			VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).unwrap()
			.generate_user_nonces(&[key]).unwrap();
		let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
			parts: super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
		}).await.unwrap().into_inner();
		let leaves = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
			.unwrap().build_signed_vtxos();
		client.register_vtxo_transactions_with_server(&leaves).await.unwrap();
		grants.push(leaves[0].id());
		signed_grants.push(leaves[0].clone());
		sources.push((input, key));
	}
	lightning.sync().await;
	rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
		protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: 90_000, htlc_vtxo_ids: vec![grants[0].to_bytes().to_vec()],
		mailbox_id: None, retry_for_secs: Some(5),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	}, &sources[0].1).await).await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let paid_source = db.read(async |t| Ok(t.query_one("
				SELECT EXISTS(SELECT 1
					FROM lightning_payment_attempt a
					JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id = a.id
					WHERE l.vtxo_id = $1 AND a.payment_hash = $2 AND a.status = 'succeeded')
			",
				&[&grants[0].to_string(), &hash.to_string()]).await?.get::<_, bool>(0))).await.unwrap();
			if paid_source && db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap() == Some(preimage) {
				break;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the actually submitted source's external invoice did not settle");
	for (index, id) in grants.iter().enumerate() {
		let count = db.read(async |t| Ok(t.query_one("
			SELECT COUNT(*)
			FROM lightning_payment_attempt_htlc_vtxo
			WHERE vtxo_id = $1
		", &[&id.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(count, if index == 0 { 1 } else { 0 }, "only the first signed grant funded the actual payment");
	}
	let height = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	srv.wait_for_sync_height(height).await;
	srv.watchmand().wait_for_sync_height(height).await;
	for (index, (input, _key)) in sources.iter().enumerate() {
		let snapshot = db.read(async |t| t.backing_subtree_snapshot(&[ark::ServerVtxo::from(input.clone()).to_bare()]).await).await.unwrap();
		let fact = snapshot.regular.iter().find(|fact| fact.state.vtxo_id == grants[index]).unwrap();
		let proposal = db.read(async |t| t.propose_expiry_ownership(fact, height).await).await.unwrap();
		if index == 0 {
			assert_eq!(proposal.disposition, OwnershipDisposition::Operator, "the source that actually funded the paid invoice is discharged");
			// A failed payment through the node next to a stored preimage that
			// no other payment or receive explains may have gone through: held.
			db.write(async |t| {
				t.execute("UPDATE lightning_payment_attempt SET status='failed',updated_at=NOW() WHERE payment_hash=$1",
					&[&hash.to_string()]).await?;
				t.execute("UPDATE htlc_vtxo SET offchain_resolution=NULL WHERE id=(SELECT id FROM vtxo WHERE vtxo_id=$1)",
					&[&grants[0].to_string()]).await?;
				Ok(())
			}).await.unwrap();
			let held = db.read(async |t| t.propose_expiry_ownership(fact, height).await).await.unwrap();
			assert_eq!(held.disposition, OwnershipDisposition::Unresolved("failed Lightning payment has a stored preimage"));
			db.write(async |t| {
				t.execute("UPDATE lightning_payment_attempt SET status='succeeded',updated_at=NOW() WHERE payment_hash=$1",
					&[&hash.to_string()]).await?;
				t.execute("UPDATE htlc_vtxo SET offchain_resolution='fulfilled' WHERE id=(SELECT id FROM vtxo WHERE vtxo_id=$1)",
					&[&grants[0].to_string()]).await?;
				Ok(())
			}).await.unwrap();
		} else {
			assert!(matches!(proposal.disposition, OwnershipDisposition::Unresolved(_)),
				"the unused, unexpired source is not discharged by another source's preimage: {:?}", proposal.disposition);
		}
	}
	{
		assert_eq!(signed_grants[1].amount(), sat(100_000));
		let unused = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, grants[1]).await).await.unwrap().unwrap();
		assert_eq!(unused.htlc.offchain_resolution, None);
		assert_eq!(unused.vtxo.oor_spent_txid, None);
		for index in [0, 1] {
			let key = sources[index].1;
			let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([signed_grants[index].clone()],
				VtxoPolicy::new_pubkey(key.public_key())).unwrap().generate_user_nonces(&[key]).unwrap();
			let mut ids = grants.clone();
			ids.extend(sources.iter().map(|(input, _)| input.id()));
			ids.extend(builder.build_unsigned_vtxos().map(|v| v.id()));
			ids.extend(builder.build_unsigned_internal_vtxos().into_iter().map(|(v, _)| v.id()));
			let txids = builder.virtual_transactions().map(|id| id.to_string()).collect::<Vec<_>>();
			let source_before = super::expiry_admission::source_rows(&db, &ids).await;
			let owners_before = super::expiry_admission::owner_rows(&db).await;
			let ids_text = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
			let committed = async || db.read(async |t| Ok(t.query_one("SELECT jsonb_build_object(
				'htlcs',(SELECT jsonb_agg(to_jsonb(h) ORDER BY h.id) FROM htlc_vtxo h WHERE h.payment_hash=$2),
				'attempts',(SELECT jsonb_agg(to_jsonb(a) ORDER BY a.id) FROM lightning_payment_attempt a WHERE a.payment_hash=$2),
				'links',(SELECT jsonb_agg(to_jsonb(l) ORDER BY l.vtxo_id) FROM lightning_payment_attempt_htlc_vtxo l JOIN lightning_payment_attempt a ON a.id=l.lightning_payment_attempt_id WHERE a.payment_hash=$2),
				'wal',(SELECT jsonb_agg(to_jsonb(s) ORDER BY s.id) FROM htlc_settlement s WHERE s.payment_hash=$2),
				'owners',(SELECT jsonb_agg(to_jsonb(c) ORDER BY c.vtxo_id) FROM coin_owner c WHERE c.vtxo_id=ANY($1)),
				'virtual',(SELECT jsonb_agg(to_jsonb(v) ORDER BY v.txid) FROM virtual_transaction v WHERE v.txid=ANY($3)),
				'allocations',(SELECT jsonb_agg(to_jsonb(c) ORDER BY c.allocation_id,c.vtxo_id) FROM expiry_allocation_coin c WHERE c.vtxo_id=ANY($1)))::TEXT",
				&[&ids_text, &hash.to_string(), &txids]).await?.get::<_, String>(0))).await.unwrap();
			let before = committed().await;
			let response = rpc.request_lightning_pay_htlc_revocation(
				super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await,
			).await;
			let error = response.expect_err("a settled hash must not be cooperatively refunded through either source");
			assert_eq!(error.code(), tonic::Code::InvalidArgument);
			assert!(error.message().ends_with("invoice has already been paid"), "{error}");
			assert_eq!(committed().await, before);
			assert_eq!(super::expiry_admission::source_rows(&db, &ids).await, source_before);
			assert_eq!(super::expiry_admission::owner_rows(&db).await, owners_before);
		}
	}

	// A recorded success accounts for the invoice. The unused source is
	// payable to its birth owner once its HTLC deadline has passed.
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(deadline.to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let snapshot = db.read(async |t| t.backing_subtree_snapshot(&[ark::ServerVtxo::from(sources[1].0.clone()).to_bare()]).await).await.unwrap();
	let fact = snapshot.regular.iter().find(|fact| fact.state.vtxo_id == grants[1]).unwrap();
	let proposal = db.read(async |t| t.propose_expiry_ownership(fact, expired).await).await.unwrap();
	let birth = db.read(async |t| t.coin_owner_mailboxes(&[grants[1]]).await).await.unwrap()[&grants[1]];
	assert_eq!(proposal.disposition, OwnershipDisposition::Customer { mailbox: birth });

	let (allocation, payout) = super::expiry_test::wait_payout(&db, grants[1]).await;
	assert_eq!(ctx.bitcoind().await_transaction(payout.compute_txid()).await, payout);
	let destination = client.fallback_script();
	let outputs = payout.output.iter().enumerate()
		.filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
	assert_eq!(outputs.len(), 1, "the unused grant has exactly one owner payout output");
	assert_eq!(outputs[0].1.value, sat(98_000));
	let owner = client.mailbox_keypair().public_key().to_string();
	let row = db.read(async |t| Ok(t.query_one("SELECT c.disposition::TEXT,c.owner_mailbox,c.gross_sat,c.fee_sat,c.payout_sat
		FROM expiry_allocation_coin c
		WHERE c.allocation_id=$1 AND c.vtxo_id=$2", &[&allocation, &grants[1].to_string()]).await?)).await.unwrap();
	assert_eq!(row.get::<_, &str>("disposition"), "customer");
	assert_eq!(row.get::<_, Option<&str>>("owner_mailbox"), Some(owner.as_str()));
	assert_eq!(row.get::<_, i64>("gross_sat"), 100_000);
	assert_eq!(row.get::<_, i64>("fee_sat"), 2_000);
	assert_eq!(row.get::<_, i64>("payout_sat"), 98_000);
	assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c
		WHERE c.vtxo_id=$1 AND c.disposition='customer'",
		&[&grants[1].to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 1);

	let tip = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(tip).await;
	srv.watchmand().wait_for_sync_height(tip).await;
	let confirmed = ctx.bitcoind().sync_client().get_tx_out(&payout.compute_txid(), outputs[0].0 as u32, Some(true)).unwrap().unwrap();
	assert!(confirmed.confirmations >= 6);
	assert_eq!(confirmed.value, sat(98_000));
	assert_eq!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap(), Some(preimage));
	let mut native = lightning.internal.grpc_client().await;
	let paid = native.list_pays(cln_rpc::ListpaysRequest { payment_hash: Some(hash.to_vec()), ..Default::default() })
		.await.unwrap().into_inner().pays;
	assert_eq!(paid.len(), 1);
	assert_eq!(paid[0].status(), cln_rpc::listpays_pays::ListpaysPaysStatus::Complete);
	assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM lightning_payment_attempt_htlc_vtxo WHERE vtxo_id=$1",
		&[&grants[1].to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
}
