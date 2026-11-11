/// The claimant and actual recipient are different wallets. A claim request
/// A receive funded over Lightning must not become collateral for an
/// unsuccessful Ark sender merely because both use the same invoice.
#[tokio::test]
async fn failed_intra_ark_promotion_does_not_bind_another_payers_grants() {
	let ctx = TestContext::new("server/failed_intra_ark_promotion_other_payer").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.receive_htlc_forward_timeout = Duration::from_secs(3600); }).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let sender = ctx.bark("sender", &srv).funded(sat(200_000)).create().await;
	let ids = sender.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let wallet = sender.client().await;
	let preimage = ark::lightning::Preimage::random();
	let hash = preimage.compute_payment_hash();
	let receiver = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let mut rpc = srv.get_public_rpc().await;
	let invoice = rpc.start_lightning_receive(protos::StartLightningReceiveRequest {
		expiry_secs: None,

		payment_hash: hash.to_vec(), amount_sat: 90_000, min_cltv_delta: 58,
		mailbox_id: Some(MailboxIdentifier::from_pubkey(receiver.public_key()).serialize()), description: None,
		claim_pubkey: Some(receiver.public_key().serialize().to_vec()),
	}).await.unwrap().into_inner().bolt11;
	lightning.sync().await;
	let mut payer = lightning.external.grpc_client().await;
	let external_invoice = invoice.clone();
	let payment = tokio::spawn(async move { payer.xpay(cln_rpc::XpayRequest {
		invstring: external_invoice, ..Default::default()
	}).await });
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	wait_for_subscription_status(&db, hash, "accepted").await;
	let tip = ctx.bitcoind().get_block_count().await as u32;
	let grants = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: receiver.public_key().serialize().to_vec(),
		htlc_recv_expiry: tip + 32, lightning_receive_anti_dos: None,
		claim_attestation: Some(ark::attestations::LightningReceiveClaimAttestation::new(
			hash, BlockHeight::new(tip + 32), &receiver).serialize()),
	}).await.unwrap().into_inner().htlc_vtxos;
	assert!(!grants.is_empty());
	wait_for_subscription_status(&db, hash, "htlcs-ready").await;
	let owner = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let owner_mailbox = MailboxIdentifier::from_pubkey(owner.public_key());
	let sources = cosign_htlc_send_to_pubkey(&ctx, &srv, &wallet, &ids, owner.public_key(),
		Some(&owner), hash, owner_mailbox).await;
	wallet.register_vtxo_transactions_with_server(&sources).await.unwrap();
	let refusal = rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
		protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: 90_000,
		htlc_vtxo_ids: sources.iter().map(|v| v.id().to_bytes().to_vec()).collect(),
		mailbox_id: None, retry_for_secs: Some(5),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	}, &owner).await).await.unwrap_err();
	assert_eq!(refusal.code(), tonic::Code::InvalidArgument);
	assert!(refusal.message().contains("already being paid"));
	let records = db.read(async |t| {
		let row = t.query_one("SELECT \
			(SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1), \
			(SELECT COUNT(*) FROM cln_xpay WHERE payment_hash=$1)", &[&hash.to_string()]).await?;
		Ok((row.get::<_, i64>(0), row.get::<_, i64>(1)))
	}).await.unwrap();
	assert_eq!(records, (0, 0), "a refused promotion records neither an attempt nor an external request");
	let keys = vec![owner; sources.len()];
	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		sources, ark::VtxoPolicy::new_pubkey(owner.public_key()),
	).unwrap().generate_user_nonces(&keys).unwrap();
	let mut cosign_request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	let owner_address = super::payout_address(&srv, ark::VtxoPolicy::new_pubkey(owner.public_key()),
		&owner, owner_mailbox).await;
	for output in cosign_request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.blinded_mailbox_id = super::blinded_mailbox(&owner_address);
	}
	let reply = rpc.request_lightning_pay_htlc_revocation(
		super::signed_package(&srv, &[&wallet], cosign_request).await,
	).await.expect("the unsuccessful sender must retain its cooperative refund").into_inner();
	let refund = builder.user_cosign(&keys,
		ark::arkoor::package::ArkoorPackageCosignResponse::try_from(reply).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(refund.iter().map(Vtxo::amount).sum::<Amount>(), sat(100_000));
	wallet.register_vtxo_transactions_with_server(&refund).await.unwrap();
	assert_eq!(lightning_subscription_status(&db, hash).await.as_deref(), Some("htlcs-ready"));
	let native = lightning.internal.hold_client().await.list(hold::ListRequest {
		constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner().invoices;
	assert_eq!(native.len(), 1);
	assert_eq!(native[0].state(), hold::InvoiceState::Accepted);
	assert!(!payment.is_finished(), "the unrelated native payer must remain held");
	payment.abort();
}

/// A failed external collection must not force a separate grant timeout.
/// The original backend cancels the invoice before the operator recovers its
/// expired backing. The physical cut contains that backing and one fee coin.
#[tokio::test]
async fn canceled_external_receive_with_preimage_recovers_backing_directly() {
	let ctx = TestContext::new("server/canceled_external_receive_recovers_backing_directly").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let payer = Arc::new(lightning.external);
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).lightningd(&lightning.internal)
		.funded(btc(10)).cfg(|cfg| { cfg.expiry_payouts = true;
			cfg.receive_htlc_forward_timeout = Duration::from_secs(120);
			cfg.vtxopool.vtxo_targets = vec![VtxoTarget { count: 2, amount: sat(100_000) }];
		}).watchmand().create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let operator_script = {
		let cfg = srv.watchmand().config();
		cfg.sweep_address.clone().unwrap().require_network(cfg.network).unwrap().script_pubkey()
	};
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let preimage = ark::lightning::Preimage::random();
	let hash = preimage.compute_payment_hash();
	let receiver = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let mut rpc = srv.get_public_rpc().await;
	let invoice = rpc.start_lightning_receive(protos::StartLightningReceiveRequest {
		expiry_secs: None,

		payment_hash: hash.to_vec(), amount_sat: 100_000, min_cltv_delta: srv.config().max_user_invoice_cltv_delta.into(),
		mailbox_id: Some(MailboxIdentifier::from_pubkey(receiver.public_key()).serialize()), description: None,
		claim_pubkey: Some(receiver.public_key().serialize().to_vec()),
	}).await.unwrap().into_inner().bolt11;
	let pay_task = {
		let payer = payer.clone();
		let invoice = invoice.clone();
		tokio::spawn(async move { payer.try_pay_bolt11(invoice).await })
	};
	wait_for_subscription_status(&db, hash, "accepted").await;
	let subscription = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await)
		.await.unwrap().unwrap();
	let incoming_expiry = subscription.lowest_incoming_htlc_expiry.unwrap();
	let claim_expiry = incoming_expiry.saturating_sub(srv.config().htlc_expiry_delta);
	let attestation = ark::attestations::LightningReceiveClaimAttestation::new(hash, claim_expiry, &receiver);
	let attestation = attestation.serialize();
	let prepare_request = protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: receiver.public_key().serialize().to_vec(),
		htlc_recv_expiry: claim_expiry.into(),
		lightning_receive_anti_dos: None,
		claim_attestation: Some(attestation),
	};
	let grants = rpc.prepare_lightning_receive_claim(prepare_request).await.unwrap().into_inner().htlc_vtxos.into_iter()
		.map(|raw| Vtxo::<Full>::deserialize(&raw).unwrap()).collect::<Vec<_>>();
	assert_eq!(grants.len(), 1);
	let grant = &grants[0];
	assert_eq!(grant.amount(), sat(100_000));
	let root = grant.chain_anchor();
	let funding = ctx.bitcoind().await_transaction(root.txid).await;
	grant.validate(&funding).unwrap();
	let backing = &funding.output[root.vout as usize];
	assert!(backing.value >= grant.amount());
	assert!(grant.expiry_height() > incoming_expiry + BlockDelta::new(2));
	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		grants.clone(), ark::VtxoPolicy::new_pubkey(receiver.public_key()),
	).unwrap().generate_user_nonces(&[receiver]).unwrap();

	let mut cosign_request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	let receiver_address = super::payout_address(&srv, ark::VtxoPolicy::new_pubkey(receiver.public_key()),
		&receiver, MailboxIdentifier::from_pubkey(receiver.public_key())).await;
	for output in cosign_request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.blinded_mailbox_id = super::blinded_mailbox(&receiver_address);
	}

	// The backend is unavailable for the settlement call. The request
	// must already be durable, but the server must not release its destination.
	let original_node = lightning.internal.grpc_client().await.getinfo(cln_rpc::GetinfoRequest {})
		.await.unwrap().into_inner();
	lightning.internal.stop().await.unwrap();
	let error = rpc.claim_lightning_receive(protos::ClaimLightningReceiveRequest {
		payment_hash: hash.to_vec(), payment_preimage: preimage.to_vec(),
		cosign_request: Some(super::signed_package(&srv, &[], cosign_request).await),
	}).wait_millis(10_000).await.expect_err("an unavailable original backend cannot collect the invoice");
	assert!(error.message().contains("could not settle hold invoice"), "unexpected error: {error}");
	assert_eq!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap(), Some(preimage));
	let resolution = db.read(async |tx| Ok(tx.query_one("SELECT h.offchain_resolution::TEXT AS resolution \
		FROM expiry_lightning_receive_claim c JOIN vtxo v ON v.vtxo_id=c.source_vtxo_id \
		JOIN htlc_vtxo h ON h.id=v.id WHERE c.source_vtxo_id=$1 AND c.payment_hash=$2",
		&[&grant.id().to_string(), &hash.to_string()]).await?.get::<_, Option<String>>(0))).await.unwrap();
	assert_eq!(resolution, None);

	srv.watchmand().stop().await.unwrap();
	srv.stop().await.unwrap();

	// Preserve the issuing node and its invoice. Cancel before captaind can
	// reconnect and retry the stored preimage against the restarted backend.
	lightning.internal.start().await.unwrap();
	let reopened_node = lightning.internal.grpc_client().await.getinfo(cln_rpc::GetinfoRequest {})
		.await.unwrap().into_inner();
	assert_eq!(reopened_node.id, original_node.id);
	let mut hold = lightning.internal.hold_client().await;
	hold.cancel(hold::CancelRequest { payment_hash: hash.to_vec() }).await.unwrap();
	let invoices = hold.list(hold::ListRequest {
		constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner().invoices;
	assert_eq!(invoices.len(), 1);
	assert_eq!(invoices[0].invoice, invoice);
	assert_eq!(invoices[0].payment_hash, hash.to_vec());
	assert_eq!(invoices[0].state(), hold::InvoiceState::Cancelled);
	assert!(invoices[0].preimage.is_none());
	lightning.internal.connect(&payer).await;
	// An xpay error can arrive before listpays reports terminal failure.
	let mut payer_state = Vec::new();
	let terminal_failure = tokio::time::timeout(Duration::from_secs(30), async {
		assert!(pay_task.await.unwrap().is_err());
		loop {
			let pays = payer.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
				bolt11: None, payment_hash: Some(hash.to_vec()), status: None,
				index: None, limit: None, start: None,
			}).await.unwrap().into_inner().pays;
			payer_state = pays.iter().map(|pay| (
				pay.status(), pay.payment_hash == hash.to_vec(), pay.preimage.is_some(),
			)).collect::<Vec<_>>();
			assert!(pays.iter().all(|pay| pay.payment_hash == hash.to_vec() && pay.preimage.is_none()
				&& pay.status() != cln_rpc::listpays_pays::ListpaysPaysStatus::Complete),
				"unexpected payer state (status, matching hash, preimage present): {:?}", payer_state);
			if !pays.is_empty() && pays.iter().all(|pay| pay.payment_hash == hash.to_vec()
				&& pay.status() == cln_rpc::listpays_pays::ListpaysPaysStatus::Failed && pay.preimage.is_none()) {
				break;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await;
	assert!(terminal_failure.is_ok(),
		"payer terminal-failure deadline: count={}, (status, matching hash, preimage present)={:?}",
		payer_state.len(), payer_state);

	// The existing daemon helper reserves fresh ports at restart.
	let grpc = lightning.internal.grpc_details().await;
	let plugin = lightning.internal.hold_details().await;
	{
		let mut cfg = srv.config_mut();
		cfg.vtxopool.vtxo_targets.clear(); // Keep this physical cut to the original pool backing.
		let node = &mut cfg.cln_array[0];
		assert_eq!(node.server_cert_path, grpc.server_cert_path);
		node.uri = grpc.uri.parse().unwrap();
		let hold = node.hold_invoice.as_mut().unwrap();
		assert_eq!(hold.server_cert_path, plugin.server_cert_path);
		hold.uri = plugin.uri.parse().unwrap();
	}
	srv.start().await.unwrap();
	srv.watchmand().start().await.unwrap();
	let tip = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(incoming_expiry.to_u32() + 1 - tip).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	wait_for_subscription_status(&db, hash, "canceled").await;

	let fee_address = srv.watchmand().wait_wallet_address().await;
	let fee_funding = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let fee_tx = ctx.bitcoind().await_transaction(fee_funding).await;
	let fee_vout = fee_tx.output.iter().position(|out|
		out.script_pubkey == fee_address.script_pubkey() && out.value == sat(100_000)
	).unwrap();
	let sponsor = OutPoint::new(fee_funding, fee_vout as u32);
	assert!(ctx.bitcoind().sync_client().get_tx_out(&sponsor.txid, sponsor.vout, Some(true))
		.unwrap().unwrap().confirmations >= 2);
	let tip = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(grant.expiry_height().to_u32() + 1 - tip).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let (allocation, parent) = async {
		loop {
			let row = db.read(async |tx| Ok(tx.query_opt("SELECT a.id,a.sweep_tx FROM expiry_allocation_state a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND a.broadcast_at IS NOT NULL ORDER BY a.id DESC LIMIT 1",
				&[&grant.id().to_string()],
			).await?)).await.unwrap();
			if let Some(row) = row {
				break (row.get::<_, i64>("id"), deserialize::<Transaction>(row.get("sweep_tx")).unwrap());
			}
			tokio::time::sleep(poll_interval()).await;
		}
	}.wait_millis(30_000).await;
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	assert_eq!(parent.version.0, 2);
	assert!(parent.vsize() <= 100_000);
	assert!(parent.input.iter().any(|input| input.previous_output == root),
		"recover the backing directly without a descendant timeout");
	assert!(parent.input.iter().all(|input|
		input.previous_output == root || input.previous_output == sponsor));
	let clause = ark::tree::signed::expiry_clause(grant.server_pubkey(), grant.expiry_height());
	let root_input = parent.input.iter().find(|input| input.previous_output == root).unwrap();
	assert!(root_input.witness.taproot_leaf_script().is_some_and(|script|
		script.version == bitcoin::taproot::LeafVersion::TapScript && script.script == clause.as_script()));
	assert!(!parent.output.is_empty());
	assert!(parent.output.iter().all(|out| out.script_pubkey == operator_script));
	let actual_fee = super::expiry_test::actual_fee(&ctx, &parent).await;
	assert!(actual_fee > Amount::ZERO);
	let sponsor_value = if parent.input.iter().any(|input| input.previous_output == sponsor) {
		sat(100_000)
	} else { Amount::ZERO };
	assert_eq!(parent.output.iter().map(|out| out.value).sum::<Amount>() + actual_fee,
		backing.value + sponsor_value);
	let recorded_fee = db.read(async |tx| Ok(tx.query_one(
		"SELECT fee_sat FROM expiry_payout_attempt WHERE allocation_id=$1 AND txid=$2",
		&[&allocation, &parent.compute_txid().to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(actual_fee.to_sat(), u64::try_from(recorded_fee).unwrap());
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	let core = ctx.bitcoind().sync_client();
	let _confirmed_block = super::expiry_test::canonical(&ctx, &parent);
	for (vout, out) in parent.output.iter().enumerate() {
		let actual = core.get_tx_out(&parent.compute_txid(), vout as u32, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 6);
		assert_eq!(actual.value, out.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), out.script_pubkey);
	}
	async {
		loop {
			let confirmed = db.read(async |tx| Ok(tx.query_one("SELECT status='confirmed' \
				FROM expiry_allocation_state WHERE id=$1", &[&allocation],
			).await?.get::<_, bool>(0))).await.unwrap();
			if confirmed { break; }
			tokio::time::sleep(poll_interval()).await;
		}
	}.wait_millis(10_000).await;
	let final_state = db.read(async |tx| Ok(tx.query_one("SELECT \
		(SELECT to_regclass('expiry_split_request') IS NULL) AS no_split_table, \
		(SELECT COUNT(*) FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id \
		JOIN vtxo v ON v.vtxo_id=e.vtxo_id WHERE v.anchor_point=$1 AND c.disposition<>'operator') AS customers, \
		(SELECT COUNT(*) FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id \
		JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE e.vtxo_id=$2 AND a.status='confirmed') AS allocations",
		&[&root.to_string(), &grant.id().to_string()],
	).await?)).await.unwrap();
	assert!(final_state.get::<_, bool>("no_split_table"));
	assert_eq!(final_state.get::<_, i64>("customers"), 0);
	assert_eq!(final_state.get::<_, i64>("allocations"), 1);
	assert_eq!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap(), Some(preimage));
}

/// A settled forwarded receive without a recorded claim still belongs to
/// the forwarding provider. The invoice destination alone transfers nothing.
#[tokio::test]
async fn settled_forwarded_receive_without_claim_pays_the_provider() {
	let ctx = TestContext::new("server/settled_forwarded_receive_without_claim").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let payer = Arc::new(lightning.external);
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).lightningd(&lightning.internal)
		.funded(btc(10)).cfg(|cfg| { cfg.expiry_payouts = true;
			cfg.vtxopool.vtxo_targets = vec![VtxoTarget { count: 2, amount: sat(100_000) }];
		}).watchmand().create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let recipient = ctx.bark("recipient", &srv).create().await;
	let address: ark::Address = recipient.address().await.parse().unwrap();
	let recipient_script = recipient.client().await.fallback_script();
	let provider_mailbox = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let grant_key = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let provider_script = ark::fallback::payout_script(provider_mailbox.public_key());

	let preimage = ark::lightning::Preimage::random();
	let hash = preimage.compute_payment_hash();
	let amount = sat(100_000);
	let mut rpc = srv.get_public_rpc().await;
	let invoice = rpc.start_lightning_receive(protos::StartLightningReceiveRequest {
		payment_hash: hash.to_vec(), amount_sat: amount.to_sat(),
		min_cltv_delta: srv.config().max_user_invoice_cltv_delta.into(),
		mailbox_id: Some(MailboxIdentifier::from_pubkey(provider_mailbox.public_key()).serialize()),
		description: None, expiry_secs: None,
		claim_pubkey: Some(grant_key.public_key().serialize().to_vec()),
	}).await.unwrap().into_inner().bolt11;
	let pay_task = {
		let payer = payer.clone();
		tokio::spawn(async move { payer.try_pay_bolt11(invoice).await })
	};
	wait_for_subscription_status(&db, hash, "accepted").await;
	let incoming_expiry = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await)
		.await.unwrap().unwrap().lowest_incoming_htlc_expiry.unwrap();
	let expiry = incoming_expiry.saturating_sub(srv.config().htlc_expiry_delta);
	let grants = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: grant_key.public_key().serialize().to_vec(),
		htlc_recv_expiry: expiry.into(),
		lightning_receive_anti_dos: None,
		claim_attestation: Some(ark::attestations::LightningReceiveClaimAttestation::new(
			hash, expiry, &grant_key).serialize()),
	}).await.unwrap().into_inner().htlc_vtxos.into_iter()
		.map(|raw| Vtxo::<Full>::deserialize(&raw).unwrap()).collect::<Vec<_>>();
	assert!(!grants.is_empty());
	assert!(grants.iter().all(|grant| grant.user_pubkey() == grant_key.public_key()));
	for grant in &grants {
		let birth_mailbox = db.read(async |tx| Ok(tx.query_one(
			"SELECT mailbox_pk FROM coin_owner WHERE vtxo_id=$1", &[&grant.id().to_string()],
		).await?.get::<_, String>(0))).await.unwrap();
		assert_eq!(birth_mailbox, provider_mailbox.public_key().to_string(),
			"the grant keeps its provider birth binding");
	}

	// The preimage reaches the settler without a cooperative claim, which
	// settles the hold invoice and pays the payer's invoice.
	assert!(db.write(async |t| t.store_htlc_settlement(preimage).await).await.unwrap().is_some());
	wait_for_subscription_status(&db, hash, "settled").await;
	pay_task.wait_millis(30_000).await.unwrap().expect("the payer's invoice is paid");
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_lightning_receive_claim \
		WHERE payment_hash=$1", &[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);

	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let latest = grants.iter().map(|grant| grant.expiry_height()).max().unwrap();
	let tip = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(latest.to_u32() + 1 - tip).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;

	// Each unclaimed grant is paid to its birth owner, not the invoice destination.
	for grant in &grants {
		let parent: Transaction = tokio::time::timeout(Duration::from_secs(60), async {
			loop {
				if let Some((_, parent)) = super::expiry_test::published_payout(&db, grant.id()).await { break parent; }
				tokio::time::sleep(poll_interval()).await;
			}
		}).await.expect("the settled grant was not paid to its provider");
		ctx.bitcoind().await_transaction(parent.compute_txid()).await;
		assert!(parent.output.iter().any(|out| out.script_pubkey == provider_script && out.value > Amount::ZERO));
		assert!(parent.output.iter().all(|out| out.script_pubkey != recipient_script));
		let owners = db.read(async |tx| Ok(tx.query("SELECT c.user_pubkey, c.owner_mailbox \
			FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1",
			&[&grant.id().to_string()]).await?)).await.unwrap();
		assert!(!owners.is_empty());
		for owner in owners {
			assert_eq!(owner.get::<_, Option<String>>("user_pubkey"), Some(grant_key.public_key().to_string()),
				"grant {} was allocated to someone other than the provider", grant.id());
			assert_eq!(owner.get::<_, Option<String>>("owner_mailbox"), Some(provider_mailbox.public_key().to_string()));
		}
	}
	let pending = rpc.check_lightning_receive(protos::CheckLightningReceiveRequest {
		hash: hash.to_vec(), wait: false,
	}).await.unwrap().into_inner();
	assert_eq!(pending.fallback_spend_state, Some(protos::VtxoSpendState::FallbackPending as i32));
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	let terminal = rpc.check_lightning_receive(protos::CheckLightningReceiveRequest {
		hash: hash.to_vec(), wait: false,
	}).await.unwrap().into_inner();
	assert_eq!(terminal.fallback_spend_state, Some(protos::VtxoSpendState::Spent as i32),
		"payment-hash status follows the provider's unclaimed grant entitlement");
	for grant in &grants {
		let birth_mailbox = db.read(async |tx| Ok(tx.query_one(
			"SELECT mailbox_pk FROM coin_owner WHERE vtxo_id=$1", &[&grant.id().to_string()],
		).await?.get::<_, String>(0))).await.unwrap();
		assert_eq!(birth_mailbox, provider_mailbox.public_key().to_string(),
			"recipient payout must not rewrite the grant holder's birth binding");
		let status = rpc.get_vtxo_status(protos::GetVtxoStatusRequest {
			vtxo_id: grant.id().to_bytes().to_vec(),
			attestation: VtxoStatusAttestation::new(grant.id(), &grant_key).serialize(),
		}).await.unwrap().into_inner();
		assert_eq!(status.owner_mailbox, provider_mailbox.public_key().serialize().to_vec());
		assert!(status.fallback_committed,
			"provider status must report its own grant payout");
	}
}
