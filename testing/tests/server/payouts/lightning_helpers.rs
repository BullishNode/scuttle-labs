// Assertion helpers shared by the upstream-adapted and Scuttle cases.

async fn cosign_htlc_send_to_pubkey(
	ctx: &TestContext,
	srv: &Captaind,
	client: &Wallet,
	input_ids: &[ark::VtxoId],
	owner: bitcoin::secp256k1::PublicKey,
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

	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		inputs.clone(), policy,
	).unwrap().generate_user_nonces(&keypairs).unwrap();

	let mut package = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in package.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		assert_eq!(VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey(), owner);
		output.mailbox_id = Some(mailbox.serialize());
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

async fn lightning_attempt_status(
	db: &Db,
	payment_hash: ark::lightning::PaymentHash,
) -> Option<String> {
	db.read(async |t| {
		Ok(t.query_opt(
			"SELECT status::text FROM lightning_payment_attempt WHERE payment_hash = $1 ORDER BY id DESC LIMIT 1",
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

async fn wait_for_attempt_status(
	db: &Db,
	payment_hash: ark::lightning::PaymentHash,
	status: &str,
) {
	for _ in 0..50 {
		if lightning_attempt_status(db, payment_hash).await.as_deref() == Some(status) {
			return;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
	panic!("payment attempt never reached {status}");
}

#[derive(Clone)]
struct CheckClaim {
	db: Db,
	other_mailbox: Keypair,
	hold: hold::hold_client::HoldClient<tonic::transport::Channel>,
	native_payment: bool,
	checked: Arc<AtomicBool>,
	// The forwarded claimant is created against this proxy before payment.
	prepare_wallet: Option<Arc<std::sync::OnceLock<Wallet>>>,
	prepare_checked: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for CheckClaim {
	async fn prepare_lightning_receive_claim(&self, upstream: &mut ArkClient, req: protos::PrepareLightningReceiveClaimRequest)
		-> Result<protos::PrepareLightningReceiveClaimResponse, tonic::Status> {
		let response = upstream.prepare_lightning_receive_claim(req.clone()).await?.into_inner();
		let Some(wallet) = &self.prepare_wallet else { return Ok(response); };
		if self.prepare_checked.load(Ordering::SeqCst) { return Ok(response); }
		let wallet = wallet.get().expect("the actual claimant is installed before its invoice is paid");
		let key = bitcoin::secp256k1::PublicKey::from_slice(&req.user_pubkey).unwrap();
		let hash = PaymentHash::try_from(req.payment_hash.as_slice()).unwrap();
		assert_eq!(lightning_subscription_status(&self.db, hash).await.as_deref(), Some("htlcs-ready"));
		let grants = response.htlc_vtxos.iter().map(|raw| Vtxo::<Full>::deserialize(raw).unwrap()).collect::<Vec<_>>();
		assert!(!grants.is_empty());
		assert!(grants.iter().all(|grant| grant.user_pubkey() == key && grant.has_all_witnesses()));
		let source_ids = grants.iter().flat_map(|grant| grant.transactions().flat_map(|step|
			step.tx.input.into_iter().map(|input| ark::VtxoId::from(input.previous_output))))
			.collect::<HashSet<_>>().into_iter().collect::<Vec<_>>();
		let before = super::expiry_admission::receive_admission_rows(&self.db, hash).await;
		let source_before = super::expiry_admission::source_rows(&self.db, &source_ids).await;
		assert!(!source_before.is_empty());
		assert_ne!(wallet.mailbox_identifier().as_pubkey(), self.other_mailbox.public_key());
		let native_request = hold::ListRequest { constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())) };
		let mut hold = self.hold.clone();
		let native_before = hold.list(native_request.clone()).await.unwrap().into_inner().invoices;
		assert_eq!(native_before.len(), 1);
		assert_eq!(native_before[0].state(), hold::InvoiceState::Accepted);
		// A prepared grant is tied to the subscription's committed claimant and
		// destination. A read of it must not change settlement or owner rows.
		let bodies = |raw: &[Vec<u8>]| raw.iter().map(|bytes|
			(Vtxo::<Full>::deserialize(bytes).unwrap().id(), bytes.clone())).collect::<std::collections::BTreeMap<_, _>>();
		assert_eq!(bodies(&response.htlc_vtxos).len(), grants.len());
		for _ in 0..2 {
			let cached = upstream.prepare_lightning_receive_claim(req.clone()).await.unwrap().into_inner();
			assert_eq!(bodies(&cached.htlc_vtxos), bodies(&response.htlc_vtxos));
			assert_eq!(cached.htlc_vtxos.len(), grants.len());
			assert_eq!(super::expiry_admission::receive_admission_rows(&self.db, hash).await, before);
			assert_eq!(super::expiry_admission::source_rows(&self.db, &source_ids).await, source_before);
			assert_eq!(hold.list(native_request.clone()).await.unwrap().into_inner().invoices, native_before);
		}
		self.prepare_checked.store(true, Ordering::SeqCst);
		Ok(response)
	}

	async fn claim_lightning_receive(&self, upstream: &mut ArkClient, req: protos::ClaimLightningReceiveRequest)
		-> Result<protos::ArkoorPackageCosignResponse, tonic::Status> {
		if self.checked.load(Ordering::SeqCst) {
			return Ok(upstream.claim_lightning_receive(req).await?.into_inner());
		}
		let output = req.cosign_request.as_ref().unwrap().parts.last().unwrap().outputs.last().unwrap();
		let key = VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey();
		let hash = PaymentHash::try_from(req.payment_hash.as_slice()).unwrap();
		let native_request = hold::ListRequest { constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())) };
		let mut hold = self.hold.clone();
		let before = super::expiry_admission::receive_admission_rows(&self.db, hash).await;
		let native_before = hold.list(native_request.clone()).await.unwrap().into_inner().invoices;
		if self.native_payment {
			assert_eq!(native_before.len(), 1);
			assert_eq!(native_before[0].state(), hold::InvoiceState::Accepted);
		}
		let mut invalid = req.clone();
		invalid.cosign_request.as_mut().unwrap().parts.last_mut().unwrap().outputs.last_mut().unwrap()
			.mailbox_id = Some(vec![0; 3]);
		assert_eq!(upstream.claim_lightning_receive(invalid).await.unwrap_err().code(), tonic::Code::InvalidArgument);
		assert_eq!(super::expiry_admission::receive_admission_rows(&self.db, hash).await, before);
		assert_eq!(hold.list(native_request.clone()).await.unwrap().into_inner().invoices, native_before,
			"a refused claim must leave the native payer's HTLC uncollected");
		let mut conflict = req.clone();
		conflict.cosign_request.as_mut().unwrap().parts.last_mut().unwrap().outputs.last_mut().unwrap().mailbox_id =
			Some(MailboxIdentifier::from_pubkey(self.other_mailbox.public_key()).serialize());
		let _accepted_response = upstream.claim_lightning_receive(req.clone()).await?.into_inner();
		wait_for_subscription_status(&self.db, hash, "settled").await;
		let settled_before = super::expiry_admission::receive_admission_rows(&self.db, hash).await;
		let settled_native = hold.list(native_request.clone()).await.unwrap().into_inner().invoices;
		assert!(upstream.claim_lightning_receive(conflict).await.is_err(),
			"a settled retry cannot change its destination");
		assert_eq!(super::expiry_admission::receive_admission_rows(&self.db, hash).await, settled_before);
		assert_eq!(hold.list(native_request).await.unwrap().into_inner().invoices, settled_native,
			"a settled retry cannot collect a second payment or change the first binding");
		let response = upstream.claim_lightning_receive(req).await.unwrap().into_inner();
		let mut replayed = super::expiry_admission::receive_admission_rows(&self.db, hash).await;
		let mut expected = settled_before;
		for rows in [&mut replayed, &mut expected] {
			rows[2] = serde_json::to_string(&super::expiry_admission::vtxo_replay_rows(serde_json::from_str(&rows[2]).unwrap())).unwrap();
		}
		assert_eq!(replayed, expected, "valid replay may touch only the VTXO update timestamp; bindings and settlement stay exact");
		// Only this replay response reaches the real wallet, which verifies its
		// partial signatures and consumes the original user nonce exactly once.
		self.checked.store(true, Ordering::SeqCst);
		Ok(response)
	}
}
