pub(super) use super::owner_helpers::{unmapped_owner_link, assert_owner_refusal, owner_rows, source_rows, vtxo_replay_rows, receive_admission_rows};


use ark::arkoor::ArkoorDestination;
use ark::arkoor::package::ArkoorPackageBuilder;
use ark::lightning::{PaymentHash, Preimage};
use ark::fallback::OwnerLink;
use ark::{ProtocolEncoding, VtxoPolicy, SECP};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::Keypair;
use bitcoin::Network;
use ark_testing::daemon::captaind::Captaind;
use server::database::Db;
use bitcoin_ext::{BlockDelta, BlockHeight};
use server_rpc::protos;

use ark_testing::{btc, sat, TestContext};

/// Both early request rejection and the final writer fence must carry the same
/// stable reason. The durable decision here is an isolated fixture;
/// allocation-operation races have separate coverage.
#[tokio::test]
async fn expiry_native_rpc_refusals_have_stable_reasons() {
	let ctx = TestContext::new("server/expiry_typed_refusals").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool()
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24)).create().await;
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
		t.query_one("INSERT INTO expiry_entitlement(vtxo_id,gross_sat) VALUES($1,$2) RETURNING id",
			&[&source.id().to_string(),&100_000_i64]).await?;
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

/// Each public issuance path rejects a missing link while its original request
/// metadata is still available, before canonical conversion or money handling.
#[tokio::test]
async fn ownership_links_are_required_at_all_eight_admissions() {
	let ctx = TestContext::new("server/owner_links_all_admissions").await;
	let srv = ctx.captaind("server").create().await;
	let mut rpc = srv.get_public_rpc().await;
	let pair = Keypair::from_seckey_slice(&SECP, &[37;32]).unwrap();
	let mailbox = Keypair::from_seckey_slice(&SECP, &[38;32]).unwrap();
	let key = pair.public_key();
	let joint = protos::OwnerProof::from(OwnerLink::new_joint(Network::Regtest,
		srv.mailbox_keypair().public_key(), &mailbox, &pair).unwrap());
	let mut version_zero = joint.clone(); version_zero.version = 0;
	let mut legacy = joint.clone(); legacy.version = 1; legacy.mailbox_signature.clear();
	let mut missing_mailbox = joint; missing_mailbox.mailbox_signature.clear();
	for proof in [None, Some(version_zero), Some(legacy), Some(missing_mailbox)] {
		let policy = VtxoPolicy::new_pubkey(key).serialize();
		let reason = if proof.is_none() { "owner_proof_required" } else { "invalid_owner_proof" };
		let destination = protos::ArkoorDestination { retired_owner_proof: None,
			total_amount: 100_000, policy: policy.clone(), owner_proof: proof.clone(),
		};
		let part = protos::ArkoorCosignRequest { outputs: vec![destination], ..Default::default() };
		let package = || protos::ArkoorPackageCosignRequest { parts: vec![part.clone()] };
		let refusal = |status: tonic::Status| {
			assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status}");
			assert_eq!(status.metadata().get("abandon-reason").unwrap().to_str().unwrap(), reason);
			assert_eq!(status.metadata().get("identifiers").unwrap().to_str().unwrap(), key.to_string());
		};
		refusal(rpc.request_board_cosign(protos::BoardCosignRequest { retired_owner_proof: None,
			user_pubkey: key.serialize().to_vec(), owner_proof: proof.clone(), ..Default::default()
		}).await.unwrap_err());
		refusal(rpc.request_arkoor_cosign(package()).await.unwrap_err());
		refusal(rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
			parts: vec![part.clone()],
		}).await.unwrap_err());
		refusal(rpc.request_lightning_pay_htlc_revocation(package()).await.unwrap_err());
		refusal(rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest { retired_owner_proof: None,
			payment_hash: vec![1; 32], user_pubkey: key.serialize().to_vec(), owner_proof: proof.clone(), ..Default::default()
		}).await.unwrap_err());
		refusal(rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
			payment_hash: vec![1; 32], payment_preimage: vec![2; 32], cosign_request: Some(package()),
		}).await.unwrap_err());
		let output = protos::VtxoRequest { retired_owner_proof: None, amount: 100_000, policy, owner_proof: proof };
		refusal(rpc.submit_payment(protos::SubmitPaymentRequest {
			vtxo_requests: vec![protos::SignedVtxoRequest { vtxo: Some(output.clone()), ..Default::default() }],
			..Default::default()
		}).await.unwrap_err());
		refusal(rpc.submit_round_participation(protos::RoundParticipationRequest {
			vtxo_requests: vec![output], ..Default::default()
		}).await.unwrap_err());
	}
	let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
	let counts = db.read(async |tx| Ok(tx.query_one("SELECT
		(SELECT count(*) FROM owner_key),
		(SELECT count(*) FROM lightning_payment_attempt),
		(SELECT count(*) FROM lightning_htlc_subscription)", &[]).await?)).await.unwrap();
	assert_eq!((counts.get::<_, i64>(0), counts.get::<_, i64>(1), counts.get::<_, i64>(2)), (0, 0, 0));
}

/// Exact invoice, source, settlement and immutable-output records. All queries
/// are scoped to this payment or its proposed keys, not background pool work.

#[derive(Clone, Copy)]
enum PackageAdmission { Arkoor, LightningSend, Revocation }

async fn package_rpc(
	rpc: &mut ark_testing::daemon::captaind::ArkClient, kind: PackageAdmission,
	request: protos::ArkoorPackageCosignRequest,
) -> Result<protos::ArkoorPackageCosignResponse, tonic::Status> {
	Ok(match kind {
		PackageAdmission::Arkoor => rpc.request_arkoor_cosign(request).await?.into_inner(),
		PackageAdmission::LightningSend => rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest { parts: request.parts }).await?.into_inner(),
		PackageAdmission::Revocation => rpc.request_lightning_pay_htlc_revocation(request).await?.into_inner(),
	})
}

async fn package_rows(db: &Db, ids: &[ark::VtxoId], txids: &[String], hash: PaymentHash) -> Vec<String> {
	let mut result = source_rows(db, ids).await;
	let hash = hash.to_string();
	result.extend(db.read(async |tx| {
		let row = tx.query_one("SELECT \
			(SELECT COALESCE(jsonb_agg(to_jsonb(v) ORDER BY txid),'[]')::TEXT FROM virtual_transaction v WHERE txid=ANY($1)), \
			(SELECT COALESCE(jsonb_agg(to_jsonb(h) ORDER BY id),'[]')::TEXT FROM htlc_vtxo h WHERE payment_hash=$2), \
			(SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id),'[]')::TEXT FROM lightning_payment_attempt a WHERE payment_hash=$2), \
			(SELECT COUNT(*)::TEXT FROM htlc_settlement WHERE payment_hash=$2)", &[&txids, &hash]).await?;
		Ok((0..4).map(|index| row.get(index)).collect::<Vec<String>>())
	}).await.unwrap());
	result
}

async fn reject_package_proofs(
	srv: &Captaind, db: &Db, kind: PackageAdmission, request: protos::ArkoorPackageCosignRequest,
	key: &Keypair, link: OwnerLink, conflict: OwnerLink, ids: &[ark::VtxoId], txids: &[String], hash: PaymentHash,
) -> protos::ArkoorPackageCosignResponse {
	let keys = request.parts.iter().flat_map(|part| part.outputs.iter().chain(&part.isolated_outputs))
		.map(|output| VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey()).collect::<Vec<_>>();
	let mut rpc = srv.get_public_rpc().await;
	let before = package_rows(db, ids, txids, hash).await;
	let owners = owner_rows(db, &keys).await;
	assert!(owners.is_empty(), "first refusal must exercise new output bindings");
	let mut invalid = request.clone();
	let output = invalid.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs))
		.rfind(|output| VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey() == key.public_key()).unwrap();
	output.owner_proof.as_mut().unwrap().signature = vec![0; 64];
	assert_owner_refusal(package_rpc(&mut rpc, kind, invalid).await.unwrap_err(), key.public_key(), bark::FallbackRefusal::InvalidOwnerProof);
	assert_eq!(package_rows(db, ids, txids, hash).await, before);
	assert_eq!(owner_rows(db, &keys).await, owners, "valid prefix bindings must roll back too");
	db.write(async |tx| tx.verify_owner_link(&srv.mailbox_keypair(), key.public_key(), &link, Network::Regtest).await).await.unwrap();
	let owners = owner_rows(db, &keys).await;
	let mut conflicting = request.clone();
	for output in conflicting.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		if VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey() == key.public_key() {
			output.owner_proof = Some(conflict.into());
		}
	}
	assert_owner_refusal(package_rpc(&mut rpc, kind, conflicting).await.unwrap_err(), key.public_key(), bark::FallbackRefusal::ConflictingOwnerBinding);
	assert_eq!(package_rows(db, ids, txids, hash).await, before);
	assert_eq!(owner_rows(db, &keys).await, owners);
	package_rpc(&mut rpc, kind, request).await.expect("the unchanged valid request must succeed after both refusals")
}

#[tokio::test]
async fn ownership_package_refusals_preserve_sources_and_isolated_prefixes() {
	let ctx = TestContext::new("server/ownership_package_refusals").await;
	let srv = ctx.captaind("server").create().await;
	let wallet = ctx.bark_sdk("sender", &srv).boarded(sat(400_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let input = wallet.spendable_vtxos().await.unwrap().pop().unwrap();
	let input = wallet.get_full_vtxo(input.id()).await.unwrap();
	let input_key = wallet.get_vtxo_key(&input).await.unwrap();
	let a = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let b = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let isolated = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let other = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let a_link = unmapped_owner_link(&srv, &a, &a).await;
	let b_link = unmapped_owner_link(&srv, &b, &b).await;
	let isolated_link = unmapped_owner_link(&srv, &isolated, &isolated).await;
	let conflict = unmapped_owner_link(&srv, &isolated, &other).await;
	let builder = ArkoorPackageBuilder::new_with_checkpoints([input.clone()], vec![
		ArkoorDestination { total_amount: sat(299_999), policy: VtxoPolicy::new_pubkey(b.public_key()) },
		ArkoorDestination { total_amount: sat(100_000), policy: VtxoPolicy::new_pubkey(a.public_key()) },
		ArkoorDestination { total_amount: sat(1), policy: VtxoPolicy::new_pubkey(isolated.public_key()) },
	]).unwrap().generate_user_nonces(&[input_key]).unwrap();
	let mut request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	assert!(request.parts.iter().any(|part| !part.isolated_outputs.is_empty()), "this case must reach isolated output proof handling");
	for output in request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		let key = VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey();
		output.owner_proof = Some(if key == a.public_key() { a_link } else if key == b.public_key() { b_link } else { isolated_link }.into());
	}
	let mut ids = builder.build_unsigned_vtxos().map(|v| v.id()).collect::<Vec<_>>(); ids.push(input.id());
	ids.extend(builder.build_unsigned_internal_vtxos().into_iter().map(|(v, _)| v.id()));
	let txids = builder.virtual_transactions().map(|txid| txid.to_string()).collect::<Vec<_>>();
	let db = Db::connect(&srv.config().postgres.clone()).await.unwrap();
	let hash = Preimage::random().compute_payment_hash();
	let response = reject_package_proofs(&srv, &db, PackageAdmission::Arkoor, request.clone(), &isolated,
		isolated_link, conflict, &ids, &txids, hash).await;
	let _ = response; // The accepted reply is lost before the user signs.
	let expected_ids = builder.build_unsigned_vtxos().map(|v| v.id()).collect::<Vec<_>>();
	let original = source_rows(&db, &ids).await;
	let mut replay = request;
	for output in replay.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		let key = VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey();
		let pair = if key == a.public_key() { &a } else if key == b.public_key() { &b } else { &isolated };
		output.owner_proof = Some(OwnerLink::new_joint(Network::Regtest, srv.mailbox_keypair().public_key(), pair, pair).unwrap().into());
	}
	let replay_response = package_rpc(&mut srv.get_public_rpc().await, PackageAdmission::Arkoor, replay).await.unwrap();
	let actual = source_rows(&db, &ids).await;
	assert_eq!(vtxo_replay_rows(actual.iter().map(|row| serde_json::from_str(row).unwrap()).collect()),
		vtxo_replay_rows(original.iter().map(|row| serde_json::from_str(row).unwrap()).collect()),
		"economic replay must preserve every row and field except its update timestamp");
	let signed = builder.user_cosign(&[input_key], replay_response.try_into().unwrap()).unwrap().build_signed_vtxos();
	assert_eq!(signed.iter().map(|v| v.id()).collect::<Vec<_>>(), expected_ids);
	assert_eq!(signed.iter().map(|v| v.amount()).sum::<bitcoin::Amount>(), sat(400_000));
	assert!(signed.iter().any(|v| v.amount() == sat(1)));
	let funding = ctx.bitcoind().await_transaction(input.chain_anchor().txid).await;
	let mut signed_txids = std::collections::BTreeSet::new();
	for output in &signed {
		output.validate(&funding).unwrap();
		for step in output.transactions() { signed_txids.insert(step.tx.compute_txid().to_string()); }
	}
	assert!(txids.iter().all(|id| signed_txids.contains(id)), "every accepted virtual transaction must remain in the signed graph");

	let source = signed.iter().find(|v| v.amount() == sat(100_000)).unwrap().clone();
	wallet.register_vtxo_transactions_with_server(&signed).await.unwrap();
	let htlc_key = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let link = unmapped_owner_link(&srv, &htlc_key, &htlc_key).await;
	let conflict = unmapped_owner_link(&srv, &htlc_key, &other).await;
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([source.clone()],
		VtxoPolicy::new_server_htlc_send(htlc_key.public_key(), hash, tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2)))
		.unwrap().generate_user_nonces(&[a]).unwrap();
	let mut request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) { output.owner_proof = Some(link.into()); }
	let mut ids = builder.build_unsigned_vtxos().map(|v| v.id()).collect::<Vec<_>>(); ids.push(source.id());
	ids.extend(builder.build_unsigned_internal_vtxos().into_iter().map(|(v, _)| v.id()));
	let txids = builder.virtual_transactions().map(|txid| txid.to_string()).collect::<Vec<_>>();
	// Keep genuine known input/nonces and a supported common HTLC policy;
	// only the untrusted wire totals overflow before package/proof validation.
	let policy = VtxoPolicy::deserialize(&request.parts[0].outputs[0].policy).unwrap();
	let overflow_builder = ArkoorPackageBuilder::new_with_checkpoints([source.clone()], vec![
		ArkoorDestination { total_amount: sat(50_000), policy: policy.clone() },
		ArkoorDestination { total_amount: sat(50_000), policy },
	]).unwrap().generate_user_nonces(&[a]).unwrap();
	let mut overflow = protos::ArkoorPackageCosignRequest::from(overflow_builder.cosign_request());
	assert_eq!(overflow.parts.len(), 1);
	assert_eq!(overflow.parts[0].outputs.len(), 2);
	assert!(overflow.parts[0].isolated_outputs.is_empty());
	for (output, amount) in overflow.parts[0].outputs.iter_mut().zip([u64::MAX, 1]) {
		output.total_amount = amount;
		output.owner_proof = Some(link.into());
	}
	let before = package_rows(&db, &ids, &txids, hash).await;
	let owners = owner_rows(&db, &[htlc_key.public_key()]).await;
	assert!(owners.is_empty());
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1",
		&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
	let error = package_rpc(&mut srv.get_public_rpc().await, PackageAdmission::LightningSend, overflow).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument);
	assert!(error.message().contains("HTLC amount overflow"), "{error}");
	assert_eq!(package_rows(&db, &ids, &txids, hash).await, before);
	assert_eq!(owner_rows(&db, &[htlc_key.public_key()]).await, owners);
	let response = reject_package_proofs(&srv, &db, PackageAdmission::LightningSend, request, &htlc_key,
		link, conflict, &ids, &txids, hash).await;
	let htlcs = builder.user_cosign(&[a], response.try_into().unwrap()).unwrap().build_signed_vtxos();
	wallet.register_vtxo_transactions_with_server(&htlcs).await.unwrap();
	assert_eq!(htlcs.iter().map(|v| v.amount()).sum::<bitcoin::Amount>(), sat(100_000));
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1", &[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0,
		"this source must be genuinely unattempted and revocable");
	let refund = Keypair::new(&ark::SECP, &mut bip39::rand::thread_rng());
	let refund_link = unmapped_owner_link(&srv, &refund, &refund).await;
	let conflict = unmapped_owner_link(&srv, &refund, &other).await;
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints(htlcs.clone(), VtxoPolicy::new_pubkey(refund.public_key()))
		.unwrap().generate_user_nonces(&vec![htlc_key; htlcs.len()]).unwrap();
	let mut request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) { output.owner_proof = Some(refund_link.into()); }
	let mut ids = builder.build_unsigned_vtxos().map(|v| v.id()).collect::<Vec<_>>(); ids.extend(htlcs.iter().map(|v| v.id()));
	ids.extend(builder.build_unsigned_internal_vtxos().into_iter().map(|(v, _)| v.id()));
	let txids = builder.virtual_transactions().map(|txid| txid.to_string()).collect::<Vec<_>>();
	let response = reject_package_proofs(&srv, &db, PackageAdmission::Revocation, request, &refund,
		refund_link, conflict, &ids, &txids, hash).await;
	let refunds = builder.user_cosign(&vec![htlc_key; htlcs.len()], response.try_into().unwrap()).unwrap().build_signed_vtxos();
	assert_eq!(refunds.iter().map(|v| v.amount()).sum::<bitcoin::Amount>(), sat(100_000));
	wallet.register_vtxo_transactions_with_server(&refunds).await.unwrap();
}
