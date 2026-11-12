// Assertion helpers shared by the upstream-adapted and Scuttle cases.

async fn cosign_htlc_send_to_pubkey(
	ctx: &TestContext,
	srv: &Captaind,
	client: &Wallet,
	input_ids: &[ark::VtxoId],
	owner: bitcoin::secp256k1::PublicKey,
	owner_key: Option<&Keypair>,
	payment_hash: ark::lightning::PaymentHash,
	mailbox: MailboxIdentifier,
) -> Vec<Vtxo<Full>> {
	let mut inputs = Vec::with_capacity(input_ids.len());
	let mut keypairs = Vec::with_capacity(input_ids.len());
	for id in input_ids {
		let vtxo = client.get_full_vtxo(*id).await.unwrap();
		keypairs.push(client.get_vtxo_key(&vtxo).await.unwrap());
		inputs.push(vtxo);
	}

	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let htlc_expiry = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let policy = ark::VtxoPolicy::new_server_htlc_send(owner, payment_hash, htlc_expiry);

	let address = if srv.config().expiry_payouts {
		Some(super::payout_address(srv, policy.clone(),
			owner_key.expect("payout-enabled HTLC output needs its coin key"), mailbox).await)
	} else { None };
	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		inputs.clone(), policy,
	).unwrap().generate_user_nonces(&keypairs).unwrap();

	let mut package = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in package.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		assert_eq!(VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey(), owner);
		output.blinded_mailbox_id = address.as_deref().map(super::blinded_mailbox).unwrap_or_default();
	}
	let mut srv_rpc = srv.get_public_rpc().await;
	let response = srv_rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: package.parts,
	}).await.expect("a valid recipient link allows a gift without making its recipient the input owner")
		.into_inner();

	let cosign_responses =
		ark::arkoor::package::ArkoorPackageCosignResponse::try_from(response).unwrap();
	builder.user_cosign(&keypairs, cosign_responses).unwrap().build_signed_vtxos()
}

async fn lightning_subscription_status(
	db: &Db,
	payment_hash: ark::lightning::PaymentHash,
) -> Option<String> {
	db.read(async |t| {
		Ok(t.query_opt(
			"SELECT status::text FROM lightning_htlc_subscription WHERE payment_hash = $1",
			&[&payment_hash.to_string()],
		).await?.map(|r| r.get(0)))
	}).await.unwrap()
}

async fn wait_for_subscription_status(
	db: &Db,
	payment_hash: ark::lightning::PaymentHash,
	status: &str,
) {
	for _ in 0..50 {
		if lightning_subscription_status(db, payment_hash).await.as_deref() == Some(status) {
			return;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
	panic!("subscription never reached {status}");
}

