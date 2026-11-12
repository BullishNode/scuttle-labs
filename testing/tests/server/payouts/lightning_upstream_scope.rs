// Payout tests that use helpers private to the upstream `lightning` test module.

fn blinded_ark_address(address: &str) -> Vec<u8> {
	let address: ark::Address = address.parse().unwrap();
	match address.delivery() {
		[ark::address::VtxoDelivery::ServerMailbox { blinded_id }] => blinded_id.as_ref().to_vec(),
		_ => panic!("test address needs one server mailbox"),
	}
}

#[tokio::test]
async fn failed_lightning_source_needs_fresh_owner_intent() {
	let ctx = TestContext::new("server/failed_lightning_source_needs_fresh_owner_intent").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let sender = ctx.bark("sender", &srv).funded(sat(500_000)).create().await;
	let inputs = sender.board_and_confirm_and_register(&ctx, sat(300_000)).await;
	let hash = ark::lightning::Preimage::random().compute_payment_hash();
	let unknown_key = bitcoin::secp256k1::SecretKey::new(&mut bip39::rand::thread_rng());
	let invoice = InvoiceBuilder::new(Currency::Regtest)
		.description("unroutable owner-intent test".into())
		.payment_hash(sha256::Hash::from_byte_array(hash.to_byte_array()))
		.payment_secret(PaymentSecret([17; 32]))
		.current_timestamp().min_final_cltv_expiry_delta(18)
		.amount_milli_satoshis(100_000_000)
		.build_signed(|message| SECP.sign_ecdsa_recoverable(message, &unknown_key)).unwrap()
		.to_string();
	let wallet = sender.client().await;
	let source = PayoutRawHtlcSend::cosign(&ctx, &srv, &wallet, &inputs, hash).await;
	let request = protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: 100_000,
		htlc_vtxo_ids: source.vtxos.iter().map(|v| v.id().to_bytes().to_vec()).collect(),
		mailbox_id: None, retry_for_secs: Some(1),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	};
	let mut rpc = srv.get_public_rpc().await;
	let foreign_key = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let foreign = ark_testing::lightning::sign_lightning_send_request(&srv, request.clone(), &foreign_key).await;
	assert_eq!(rpc.initiate_lightning_payment(foreign).await.unwrap_err().code(),
		tonic::Code::InvalidArgument);
	let signed = ark_testing::lightning::sign_lightning_send_request(&srv, request.clone(), &source.keypair).await;
	rpc.initiate_lightning_payment(signed.clone()).await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	wait_for_latest_attempt_status(&db, hash, "failed").await;

	let missing = rpc.initiate_lightning_payment(request.clone()).await.unwrap_err();
	assert_eq!(missing.code(), tonic::Code::InvalidArgument);
	let foreign = ark_testing::lightning::sign_lightning_send_request(&srv, request.clone(), &foreign_key).await;
	let wrong_key = rpc.initiate_lightning_payment(foreign).await.unwrap_err();
	assert_eq!(wrong_key.code(), tonic::Code::InvalidArgument);
	let mut changed_amount = signed.clone();
	changed_amount.payment_amount_sat += 1;
	assert!(rpc.initiate_lightning_payment(changed_amount).await.unwrap_err()
		.message().contains("source owner signature"));
	let mut changed_retry = signed.clone();
	changed_retry.retry_for_secs = Some(2);
	assert!(rpc.initiate_lightning_payment(changed_retry).await.unwrap_err()
		.message().contains("source owner signature"));
	let mut changed_nonce = signed.clone();
	changed_nonce.intent_nonce.as_mut().unwrap()[0] ^= 1;
	assert!(rpc.initiate_lightning_payment(changed_nonce).await.unwrap_err()
		.message().contains("source owner signature"));
	let replay = rpc.initiate_lightning_payment(signed).await.unwrap_err();
	assert!(replay.message().contains("already used"), "{replay}");
	let count = db.read(async |t| Ok(t.query_one(
		"SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1",
		&[&hash.to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(count, 1, "rejected calls created another attempt");

	let fresh = ark_testing::lightning::sign_lightning_send_request(&srv, request, &source.keypair).await;
	rpc.initiate_lightning_payment(fresh).await.unwrap();
	let count = db.read(async |t| Ok(t.query_one(
		"SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1",
		&[&hash.to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(count, 2, "the owner must still be able to retry a failed send");
	wait_for_latest_attempt_status(&db, hash, "failed").await;
	source.revoke(&srv).await.expect("failed source remains refundable");
}

#[tokio::test]
async fn payout_unrelated_preimage_does_not_claim_failed_send_source() {
	unrelated_preimage_does_not_claim_failed_send_source_in_mode(true).await;
}

async fn unrelated_preimage_does_not_claim_failed_send_source_in_mode(payout: bool) {
	let ctx = TestContext::new(if payout {
		"server/payout_unrelated_preimage_does_not_claim_failed_send_source"
	} else { "server/unrelated_preimage_does_not_claim_failed_send_source" }).await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let ark_ln = lightning.internal;
	let eve_ln = Arc::new(lightning.external);
	let srv = ctx.captaind("server").lightningd(&ark_ln).watchmand()
		.watchmand_cfg(|cfg| {
			cfg.watchman.reaction_interval = Duration::from_millis(100);
			cfg.watchman.sweep_interval = Duration::from_millis(100);
		})
		.cfg(move |cfg| cfg.expiry_payouts = payout).create().await;
	ctx.fund_captaind(&srv, btc(10)).await;
	srv.wait_for_vtxopool(&ctx).await;
	let victim = ctx.bark("victim", &srv).funded(btc(3)).create().await;
	victim.board_and_confirm_and_register(&ctx, btc(2)).await;
	ark_ln.wait_for_block_sync().await;
	eve_ln.wait_for_block_sync().await;

	let preimage = ark::lightning::Preimage::random();
	let hash = preimage.compute_payment_hash();
	let mut hold = eve_ln.hold_client().await;
	let invoice = hold.invoice(hold::InvoiceRequest {
		payment_hash: hash.to_vec(), amount_msat: btc(1).to_msat(),
		description: Some(hold::invoice_request::Description::Memo("failed original".into())),
		min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
	}).await.unwrap().into_inner().bolt11;
	victim.try_pay_lightning(&invoice, None, false).await.unwrap();
	wait_for_hold_invoice_accepted(&mut hold, hash).await;
	let client = victim.client().await;
	let [source_id] = htlc_send_vtxo_ids(&client, hash).await.try_into()
		.expect("one original HTLC source");
	let source = client.get_full_vtxo(source_id).await.unwrap();

	let key = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let mut rpc = srv.get_public_rpc().await;
	let second = rpc.start_lightning_receive(protos::StartLightningReceiveRequest {
		 payment_hash: hash.to_vec(), amount_sat: 100_000,
		min_cltv_delta: 6,
		mailbox_id: payout.then(|| ark::mailbox::MailboxIdentifier::from_pubkey(key.public_key()).serialize()),
		description: None, expiry_secs: None,
		claim_pubkey: payout.then(|| key.public_key().serialize().to_vec()),
		ark_address_hint: false,
	}).await.unwrap().into_inner().bolt11;
	let paying = {
		let eve_ln = eve_ln.clone();
		tokio::spawn(async move { eve_ln.try_pay_bolt11(second).await })
	};
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	wait_for_subscription_status(&db, hash, "accepted").await;
	let sub = db.read(async |t| t.get_htlc_subscription_by_payment_hash(hash).await)
		.await.unwrap().unwrap();
	let open = db.read(async |t| t.get_open_htlc_subscription_for_node_by_payment_hash(sub.lightning_node_id, &hash).await)
		.await.unwrap().unwrap();
	assert_eq!(open.id, sub.id);
	let expiry = sub.lowest_incoming_htlc_expiry.unwrap()
		.saturating_sub(srv.config().htlc_expiry_delta);
	let grants = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(),
		user_pubkey: key.public_key().serialize().to_vec(),
		htlc_recv_expiry: expiry.into(),
		lightning_receive_anti_dos: None,
		claim_attestation: payout.then(|| LightningReceiveClaimAttestation::new(hash, expiry, &key).serialize()),
	}).await.unwrap().into_inner().htlc_vtxos.into_iter()
		.map(|bytes| Vtxo::<Full>::deserialize(&bytes).unwrap()).collect::<Vec<_>>();
	let keys = vec![key; grants.len()];
	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		grants, ark::VtxoPolicy::new_pubkey(key.public_key()),
	).unwrap().generate_user_nonces(&keys).unwrap();
	let mut package = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	let address = if payout {
		let info = srv.ark_info().await;
		Some(ark::Address::builder()
			.testnet(srv.config().network != bitcoin::Network::Bitcoin)
			.server_pubkey(info.server_pubkey)
			.policy(ark::VtxoPolicy::new_pubkey(key.public_key()))
			.mailbox(info.mailbox_pubkey,
				ark::mailbox::MailboxIdentifier::from_pubkey(key.public_key()), &key).unwrap()
			.into_address().unwrap().to_string())
	} else { None };
	for output in package.parts.iter_mut().flat_map(|part|
		part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.blinded_mailbox_id = address.as_deref().map(blinded_ark_address).unwrap_or_default();
	}
	rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
		payment_hash: hash.to_vec(), payment_preimage: preimage.to_vec(),
		cosign_request: Some(package),
	}).await.unwrap();
	assert!(paying.wait_millis(10_000).await.unwrap().is_ok());
	assert!(is_preimage_recorded(&db, hash).await);

	hold.cancel(hold::CancelRequest { payment_hash: hash.to_vec() }).await.unwrap();
	let mut ark_rpc = ark_ln.grpc_client().await;
	async {
		loop {
			let pays = ark_rpc.list_pays(cln_rpc::ListpaysRequest {
				payment_hash: Some(hash.to_vec()), ..Default::default()
			}).await.unwrap().into_inner().pays;
			if pays.iter().any(|pay|
				pay.status() == cln_rpc::listpays_pays::ListpaysPaysStatus::Failed) { break; }
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}.wait_millis(10_000).await;
	assert_eq!(db.read(async |t| t.source_settlement(hash, source_id).await).await.unwrap(),
		server::database::ln::SourceSettlement::Unknown);

	let mut seen = HashSet::new();
	for item in source.transactions() {
		if seen.insert(item.tx.compute_txid()) {
			ctx.broadcast_cpfp(&item.tx).await;
			ctx.generate_blocks(1).await;
		}
	}
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	srv.watchmand().wait_for_sync_height(tip).await;
	let tip = ctx.generate_blocks(source.exit_delta().to_u32() + 1).await;
	srv.watchmand().wait_for_sync_height(tip).await;
	srv.watchmand().trigger_sweep().await;
	tokio::time::sleep(Duration::from_secs(2)).await;
	assert!(server::bitcoind::get_mempool_spending_tx(
		&ctx.bitcoind().async_client(), source.point(),
	).await.unwrap().is_none(), "watchman tried to claim a failed source using another payment's preimage");
	assert!(ctx.bitcoind().sync_client().get_tx_out(
		&source.point().txid, source.point().vout, Some(true),
	).unwrap().is_some(), "failed source collateral was spent on chain");

	// The customer can still take the timeout branch of the same contract.
	let expiry = source.policy().as_server_htlc_send().unwrap().htlc_expiry;
	let now = ctx.bitcoind().get_block_count().await as u32;
	if now <= expiry.to_u32() {
		ctx.generate_blocks(expiry.to_u32() + 1 - now).await;
	}
	victim.sync().await;
	assert_eq!(client.stuck_failed_lightning_sends().await.unwrap().len(), 1);
	client.allow_lightning_send_to_exit(hash).await.unwrap();
	victim.sync().await;
	assert!(!victim.list_exits().await.is_empty(), "failed sender has no timeout exit");
	complete_exit(&ctx, &victim).await;
	let before = victim.onchain_balance().await;
	victim.claim_all_exits(victim.get_onchain_address().await).await;
	ctx.generate_blocks(1).await;
	victim.progress_exit().await;
	assert!(victim.onchain_balance().await >= before + sat(90_000_000),
		"customer timeout claim did not return funds on chain");
}

/// The first recorded claim fixes its output and mailbox before registration.
#[tokio::test]
async fn server_lightning_receive_claim_output_mailbox_is_immutable() {
	require_bark_version!(> "0.5.0");
	let ctx = TestContext::new("lightningd/server_lightning_receive_claim_destination_bound").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.expiry_payouts = true).create().await;
	srv.wait_for_vtxopool(&ctx).await;

	let recipient = ctx.bark("intended-recipient", &srv).create().await;
	let address: ark::Address = recipient.address().await.parse().unwrap();
	let preimage = ark::lightning::Preimage::from([23u8; 32]);
	let hash = preimage.compute_payment_hash();
	let amount = sat(100_000);
	let net = amount - srv.config().fees.lightning_receive.calculate(amount).unwrap();
	// The grant starts with the provider's mailbox; the recorded claim later
	// chooses a different recipient address.
	let provider_mailbox = Keypair::from_seckey_slice(&SECP, &[60; 32]).unwrap();
	let grant_key = Keypair::from_seckey_slice(&SECP, &[61; 32]).unwrap();
	assert_ne!(grant_key.public_key(), address.policy().user_pubkey());
	let info = srv.ark_info().await;
	let provider_address = ark::Address::builder()
		.testnet(srv.config().network != bitcoin::Network::Bitcoin)
		.server_pubkey(info.server_pubkey)
		.policy(ark::VtxoPolicy::new_pubkey(grant_key.public_key()))
		.mailbox(info.mailbox_pubkey,
			ark::mailbox::MailboxIdentifier::from_pubkey(provider_mailbox.public_key()), &grant_key).unwrap()
		.into_address().unwrap().to_string();
	let mut rpc = srv.get_public_rpc().await;
	let invoice = rpc.start_lightning_receive(protos::StartLightningReceiveRequest {
		payment_hash: hash.to_vec(), amount_sat: amount.to_sat(), min_cltv_delta: 40,
		mailbox_id: Some(ark::mailbox::MailboxIdentifier::from_pubkey(provider_mailbox.public_key()).serialize()),
		description: None, expiry_secs: None,
		claim_pubkey: Some(grant_key.public_key().serialize().to_vec()),
		ark_address_hint: false,
	}).await.unwrap().into_inner().bolt11;

	let payer = lightning.external;
	let pay_task = tokio::spawn(async move { payer.try_pay_bolt11(invoice).await });
	rpc.check_lightning_receive(protos::CheckLightningReceiveRequest {
		hash: hash.to_vec(), wait: true,
	}).wait_millis(10_000).await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let sub = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await)
		.await.unwrap().unwrap();
	// The grant belongs to the provider until a claim is recorded.
	assert_eq!(sub.claim_pubkey, Some(grant_key.public_key()));
	let lowest = sub.lowest_incoming_htlc_expiry.unwrap();
	let expiry = lowest.saturating_sub(srv.config().htlc_expiry_delta);

	// A third key, with its own attestation, cannot take the grant.
	let third_key = Keypair::from_seckey_slice(&SECP, &[63; 32]).unwrap();
	let refused = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: third_key.public_key().serialize().to_vec(),
		htlc_recv_expiry: expiry.into(),
		lightning_receive_anti_dos: None,
		claim_attestation: Some(LightningReceiveClaimAttestation::new(hash, expiry, &third_key).serialize()),
	}).await.expect_err("a third key took the grant");
	assert_eq!(refused.code(), tonic::Code::InvalidArgument);
	assert!(refused.message().contains("claim key"), "{refused}");
	assert_eq!(count_htlc_recv_vtxos(&db).await, 0);

	let granted = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: grant_key.public_key().serialize().to_vec(),
		htlc_recv_expiry: expiry.into(),
		lightning_receive_anti_dos: None,
		claim_attestation: Some(LightningReceiveClaimAttestation::new(hash, expiry, &grant_key).serialize()),
	}).await.unwrap().into_inner().htlc_vtxos.into_iter()
		.map(|bytes| Vtxo::<Full>::deserialize(&bytes).unwrap()).collect::<Vec<_>>();
	assert!(!granted.is_empty());

	let valid_builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		granted.clone(), address.policy().clone(),
	).unwrap().generate_user_nonces(&[grant_key]).unwrap();
	let mut valid_package = protos::ArkoorPackageCosignRequest::from(valid_builder.cosign_request());
	for output in valid_package.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.blinded_mailbox_id = blinded_ark_address(&address.to_string());
	}
	let mut changed_mailbox = valid_package.clone();
	let (_, recipient_pair) = recipient.client().await.pubkey_keypair(&address.policy().user_pubkey())
		.await.unwrap().unwrap();
	let altered_address = ark::Address::builder()
		.testnet(srv.config().network != bitcoin::Network::Bitcoin)
		.server_pubkey(info.server_pubkey)
		.policy(address.policy().clone())
		.mailbox(info.mailbox_pubkey,
			ark::mailbox::MailboxIdentifier::from_pubkey(provider_mailbox.public_key()), &recipient_pair).unwrap()
		.into_address().unwrap().to_string();
	for output in changed_mailbox.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.blinded_mailbox_id = blinded_ark_address(&altered_address);
	}
	// A committed expiry fence must refuse the claim before the preimage
	// becomes durable. Remove this synthetic fence to complete the claim.
	let source_id = granted[0].id().to_string();
	db.write(async |tx| { tx.execute("INSERT INTO expiry_entitlement(vtxo_id) VALUES($1)",
		&[&source_id]).await?; Ok(()) }).await.unwrap();
	let decided = rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
		payment_hash: hash.to_vec(), payment_preimage: preimage.to_vec(),
		cosign_request: Some(valid_package.clone()),
	}).await.expect_err("a decided grant released its preimage");
	assert_eq!(decided.code(), tonic::Code::InvalidArgument, "{decided}");
	assert_eq!(decided.metadata().get("abandon-reason").unwrap().to_str().unwrap(), "decided_for_fallback");
	assert!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	assert_eq!(db.read(async |tx| Ok(tx.query_one(
		"SELECT COUNT(*) FROM expiry_lightning_receive_claim WHERE payment_hash=$1",
		&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
	db.write(async |tx| { tx.execute("DELETE FROM expiry_entitlement WHERE vtxo_id=$1",
		&[&source_id]).await?; Ok(()) }).await.unwrap();
	let response = rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
		payment_hash: hash.to_vec(), payment_preimage: preimage.to_vec(),
		cosign_request: Some(valid_package),
	}).await.expect("the intended recipient should claim").into_inner();
	let signed = valid_builder.user_cosign(&[grant_key], response.try_into().unwrap()).unwrap();
	let outputs = signed.build_signed_vtxos();
	assert!(!outputs.is_empty());
	assert!(outputs.iter().all(|v| v.amount() == net
		&& v.policy().user_pubkey() == address.policy().user_pubkey() && v.has_all_witnesses()));
	// The payout engine reads the payee from these rows: one per grant, each
	// paying the recipient.
	let claims = db.read(async |tx| Ok(tx.query("SELECT source_vtxo_id, output_vtxo, output_mailbox \
		FROM expiry_lightning_receive_claim WHERE payment_hash=$1", &[&hash.to_string()]).await?)).await.unwrap();
	assert_eq!(claims.len(), granted.len());
	for claim in &claims {
		assert!(granted.iter().any(|grant| grant.id().to_string() == claim.get::<_, String>("source_vtxo_id")));
		let output = Vtxo::<Full>::deserialize(claim.get("output_vtxo")).unwrap();
		assert_eq!(output.policy().user_pubkey(), address.policy().user_pubkey());
		assert_eq!(claim.get::<_, &str>("output_mailbox"), recipient.client().await.mailbox_identifier().as_pubkey().to_string());
	}
	// Reusing the same output bytes with another payout mailbox is refused
	// before the final coin row exists.
	let mailbox_refused = rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
		payment_hash: hash.to_vec(), payment_preimage: preimage.to_vec(), cosign_request: Some(changed_mailbox),
	}).await.expect_err("a retry changed the recorded claim mailbox");
	assert!(mailbox_refused.message().contains("existing receive output differs from its claim mailbox"), "{mailbox_refused}");
	// A retry cannot replace the output key either.
	let retry_builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		granted.clone(), ark::VtxoPolicy::new_pubkey(grant_key.public_key()),
	).unwrap().generate_user_nonces(&[grant_key]).unwrap();
	let mut retry = protos::ArkoorPackageCosignRequest::from(retry_builder.cosign_request());
	for output in retry.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.blinded_mailbox_id = blinded_ark_address(&provider_address);
	}
	let refused = rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
		payment_hash: hash.to_vec(), payment_preimage: preimage.to_vec(), cosign_request: Some(retry),
	}).await.expect_err("a retry changed the recorded claim output");
	assert!(refused.message().contains("receive claim conflicts"), "{refused}");
	rpc.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: outputs.iter().map(|v| v.serialize()).collect(),
	}).await.expect("the intended output can be registered");
	let recipient_mailbox = recipient.client().await.mailbox_identifier().as_pubkey().to_string();
	for output in &outputs {
		let actual = db.read(async |tx| Ok(tx.query_one(
			"SELECT mailbox_pk FROM coin_owner WHERE vtxo_id=$1", &[&output.id().to_string()],
		).await?.get::<_, String>(0))).await.unwrap();
		assert_eq!(actual, recipient_mailbox);
	}
	for grant in &granted {
		let actual = db.read(async |tx| Ok(tx.query_one(
			"SELECT mailbox_pk FROM coin_owner WHERE vtxo_id=$1", &[&grant.id().to_string()],
		).await?.get::<_, String>(0))).await.unwrap();
		assert_eq!(actual, provider_mailbox.public_key().to_string());
	}
	assert_eq!(db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await)
		.await.unwrap().unwrap().status, server::database::ln::LightningHtlcSubscriptionStatus::Settled);
	assert!(pay_task.wait_millis(30_000).await.unwrap().is_ok());
}

// Payout-mode forms of upstream helpers: they sign the send intent and name
// each output's payout mailbox.
/// HTLC-send vtxos that a client cosigned straight through the server RPC,
/// keyed to a fresh keypair it holds, so a test can initiate and revoke them
/// for any payment hash.
struct PayoutRawHtlcSend {
	keypair: Keypair,
	vtxos: Vec<Vtxo<Full>>,
}

impl PayoutRawHtlcSend {
	/// Lock `input_ids` of `client` into registered HTLC-send vtxos for
	/// `payment_hash`, without initiating a payment.
	async fn cosign(
		ctx: &TestContext,
		srv: &Captaind,
		client: &Wallet,
		input_ids: &[ark::VtxoId],
		payment_hash: PaymentHash,
	) -> PayoutRawHtlcSend {
		let keypair = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
		let vtxos = payout_cosign_htlc_send_to_pubkey(
			ctx, srv, client, input_ids, keypair.public_key(), Some(&keypair), payment_hash,
			ark::mailbox::MailboxIdentifier::from_pubkey(keypair.public_key()),
		).await;

		let mut srv_rpc = srv.get_public_rpc().await;
		srv_rpc.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
			vtxos: vtxos.iter().map(|v| v.serialize().to_vec()).collect(),
		}).await.unwrap();

		PayoutRawHtlcSend { keypair, vtxos }
	}

	async fn revoke(&self, srv: &Captaind) -> Result<(), tonic::Status> {
		payout_revoke_htlc_vtxos(srv, self.vtxos.clone(), &vec![self.keypair; self.vtxos.len()],
			ark::mailbox::MailboxIdentifier::from_pubkey(self.keypair.public_key())).await
	}
}

async fn payout_cosign_htlc_send_to_pubkey(
	ctx: &TestContext,
	srv: &Captaind,
	client: &Wallet,
	input_ids: &[ark::VtxoId],
	owner: bitcoin::secp256k1::PublicKey,
	owner_key: Option<&Keypair>,
	payment_hash: ark::lightning::PaymentHash,
	mailbox: ark::mailbox::MailboxIdentifier,
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
		Some(crate::payouts::payout_address(srv, policy.clone(),
			owner_key.expect("payout-enabled HTLC output needs its coin key"), mailbox).await)
	} else { None };
	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		inputs.clone(), policy,
	).unwrap().generate_user_nonces(&keypairs).unwrap();

	let mut package = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in package.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		assert_eq!(ark::VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey(), owner);
		output.blinded_mailbox_id = address.as_deref().map(crate::payouts::blinded_mailbox).unwrap_or_default();
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

/// Send a "claim all" revocation of `htlc_vtxos`, signed with `keypairs`,
/// straight to the server.
async fn payout_revoke_htlc_vtxos(
	srv: &Captaind,
	htlc_vtxos: Vec<Vtxo<Full>>,
	keypairs: &[Keypair],
	mailbox: ark::mailbox::MailboxIdentifier,
) -> Result<(), tonic::Status> {
	// Any pubkey works for the revocation output; the tests never claim it.
	let revocation_key = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let revocation_pubkey = revocation_key.public_key();
	let policy = ark::VtxoPolicy::new_pubkey(revocation_pubkey);
	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		htlc_vtxos,
		policy.clone(),
	).unwrap().generate_user_nonces(keypairs).unwrap();

	let mut cosign_request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	if srv.config().expiry_payouts {
		let address = crate::payouts::payout_address(srv, policy, &revocation_key, mailbox).await;
		for output in cosign_request.parts.iter_mut().flat_map(|part|
			part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
			assert_eq!(ark::VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey(), revocation_pubkey);
			output.blinded_mailbox_id = crate::payouts::blinded_mailbox(&address);
		}
	}

	let mut srv_rpc = srv.get_public_rpc().await;
	srv_rpc.request_lightning_pay_htlc_revocation(cosign_request).await.map(|_| ())
}

/// Waits until the newest payment attempt for `payment_hash` reaches `status`.
async fn wait_for_latest_attempt_status(db: &Db, payment_hash: ark::lightning::PaymentHash, status: &str) {
	for _ in 0..50 {
		let current = db.read(async |t| Ok(t.query_opt(
			"SELECT status::text FROM lightning_payment_attempt WHERE payment_hash = $1 ORDER BY id DESC LIMIT 1",
			&[&payment_hash.to_string()],
		).await?.map(|r| r.get::<_, String>(0)))).await.unwrap();
		if current.as_deref() == Some(status) {
			return;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
	panic!("payment attempt never reached {status}");
}
