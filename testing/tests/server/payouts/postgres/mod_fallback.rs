
/// An existing successful attempt linked to one signed HTLC-send source.
async fn succeeded_linked_htlc_send(name: &str) -> (TestContext, Db, ServerVtxo<ark::vtxo::Full>, i64) {
	use server::database::ln::LightningPaymentStatus;

	let mut ctx = TestContext::new_minimal(name).await;
	ctx.init_central_postgres().await;
	let postgres_cfg = ctx.new_postgres(&ctx.test_name).await;

	Db::create_with_payouts(&postgres_cfg, true).await.expect("Database created");
	let db = Db::connect(&postgres_cfg).await.expect("Connected to database");

	let pubkey = PublicKey::from_str(DUMMY_PUBKEY).unwrap();
	let (node_id, _) = db.write(async |t| t.register_lightning_node(&pubkey).await).await.unwrap();

	// A signed spendable HTLC-send source linked to its existing attempt.
	let vtxo = ServerVtxo::from(VTXO_VECTORS.arkoor_htlc_out_vtxo.clone());
	let htlc = VTXO_VECTORS.arkoor_htlc_out_vtxo.policy().as_server_htlc_send().unwrap();
	let hash = htlc.payment_hash;
	db.write(async |t| {
		t.execute_vtxo_tree_update(VtxoTreeUpdate::new().insert_spendable_vtxos([vtxo.clone()])
			.bind_owner_mailboxes([(vtxo.id(), super::test_server_mailbox_key().public_key())])?).await?;
		htlc_vtxo::create_htlc_vtxos(t, &[(vtxo.id(), hash, htlc.htlc_expiry)], HtlcDirection::Incoming).await
	}).await.unwrap();
	let secp = Secp256k1::new();
	let invoice_key = SecretKey::from_slice(&[41; 32]).unwrap();
	let bolt11 = InvoiceBuilder::new(Currency::Regtest)
		.description("source-matching coarse-status negative".into())
		.payment_hash(hash.to_sha256_hash())
		.payment_secret(PaymentSecret([42; 32]))
		.current_timestamp().min_final_cltv_expiry_delta(18)
		.amount_milli_satoshis(vtxo.amount().to_sat() * 1_000)
		.build_signed(|message| secp.sign_ecdsa_recoverable(message, &invoice_key)).unwrap();
	let invoice = Invoice::Bolt11(bolt11.clone());
	assert_eq!(invoice.payment_hash(), hash, "negative must bind the actual signed source hash");
	let source = db.read(async |tx| Ok(tx.query_one("SELECT h.direction::TEXT,h.payment_hash \
		FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id WHERE v.vtxo_id=$1", &[&vtxo.id().to_string()]).await?))
		.await.unwrap();
	assert_eq!(source.get::<_, &str>("direction"), "incoming");
	assert_eq!(source.get::<_, &str>("payment_hash"), hash.to_string());

	// Open attempt linked to the HTLC-send vtxo.
	legacy_payment_attempt(&db, node_id, &invoice, &[vtxo.id()]).await;

	let attempt = db.read(async |t| t.get_open_lightning_payment_attempt_by_payment_hash(
		(&bolt11).into()
	).await).await.unwrap().expect("open attempt present");

	// Persist the upstream attempt outcome before consuming its source.
	db.write(async |t| t.update_lightning_payment_attempt_result(
		&attempt, LightningPaymentStatus::Succeeded, None, None,
	).await).await.unwrap().expect("attempt should have transitioned to Succeeded");

	assert!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	(ctx, db, vtxo, attempt.id)
}

/// An existing successful attempt consumes only its linked signed HTLC sources.
#[tokio::test]
async fn successful_attempt_consumes_its_linked_htlc_send() {
	let (ctx, db, vtxo, attempt) = succeeded_linked_htlc_send("postgresd/successful_attempt_consumes_linked_source").await;
	db.write(async |t| t.mark_htlc_send_vtxos_ln_spent(attempt).await).await.unwrap();

	// The existing source link identifies the coin consumed by this success.
	let db_client = ctx.postgres_manager().database_client(Some(&ctx.test_name)).await;
	let row = db_client.query_one(
		"SELECT spend_state::text AS spend_state FROM vtxo WHERE vtxo_id = $1",
		&[&vtxo.id().to_string()],
	).await.unwrap();
	let spend_state: String = row.get("spend_state");
	assert_eq!(spend_state, "ln-spent", "the successful attempt consumes its linked source");
}

/// The success writer waits behind an expiry decision on its source, then
/// leaves the decided coin spent: one backing coin keeps one obligation.
#[tokio::test]
async fn success_after_expiry_decision_never_consumes_the_decided_source() {
	let (_ctx, db, vtxo, attempt) = succeeded_linked_htlc_send("postgresd/success_after_expiry_decision").await;
	let id = vtxo.id().to_string();
	let competing = db.clone();
	let (pid_send, pid_recv) = tokio::sync::oneshot::channel();
	let writer = db.write(async |t| {
		t.query_one("SELECT vtxo_id FROM vtxo WHERE vtxo_id=$1 FOR UPDATE", &[&id]).await?;
		let writer = tokio::spawn(async move {
			competing.write(async |t| {
				pid_send.send(t.query_one("SELECT pg_backend_pid()", &[]).await?.get::<_, i32>(0)).unwrap();
				t.mark_htlc_send_vtxos_ln_spent(attempt).await
			}).await
		});
		let pid = pid_recv.await?;
		while !t.query_one("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND NOT granted)", &[&pid]).await?.get::<_, bool>(0) {
			tokio::time::sleep(std::time::Duration::from_millis(10)).await;
		}
		t.execute("INSERT INTO expiry_entitlement(vtxo_id) VALUES($1)", &[&id]).await?;
		t.execute("UPDATE vtxo SET spend_state='spent',updated_at=NOW() WHERE vtxo_id=$1", &[&id]).await?;
		Ok(writer)
	}).await.unwrap();
	assert_eq!(writer.await.unwrap().unwrap(), vec![vtxo.id()], "the decided source is reported, not consumed");
	let state = db.read(async |t| Ok(t.query_one("SELECT spend_state::TEXT FROM vtxo WHERE vtxo_id=$1", &[&id]).await?
		.get::<_, String>(0))).await.unwrap();
	assert_eq!(state, "spent");
}

async fn legacy_payment_attempt(db: &Db, node_id: i64, invoice: &Invoice, sources: &[VtxoId]) {
	let principal = i64::try_from(invoice.amount_msat().expect("fixed invoice fixture")).unwrap();
	let hash = invoice.payment_hash().to_string();
	let ids = sources.iter().map(ToString::to_string).collect::<Vec<_>>();
	db.write(async |t| {
		let id = t.query_one("INSERT INTO lightning_payment_attempt( \
			lightning_node_id,payment_hash,amount_msat,sender_mailbox_id,status,block_height, \
			user_fee_sat,user_agent,retry_for_secs,created_at,updated_at,lightning_htlc_subscription_id) \
			VALUES($1,$2,$3,NULL,'requested',1,1,'bark',60,NOW(),NOW(),NULL) RETURNING id",
			&[&node_id, &hash, &principal]).await?.get::<_, i64>(0);
		if !ids.is_empty() {
			t.execute("INSERT INTO lightning_payment_attempt_htlc_vtxo(lightning_payment_attempt_id,vtxo_id) \
				SELECT $1,unnest($2::TEXT[])", &[&id, &ids]).await?;
		}

		Ok(())
	}).await.unwrap();
}

/// Existing source facts and a consistent WAL are both required. These
/// explicit PostgreSQL metadata fixtures do not claim a native payment.
#[tokio::test]
async fn settlement_source_facts_hold_unknown_and_require_matching_wal() {
	let mut ctx = TestContext::new_minimal("postgresd/settlement_source_facts").await;
	ctx.init_central_postgres().await;
	let config = ctx.new_postgres(&ctx.test_name).await;
	Db::create_with_payouts(&config, true).await.unwrap();
	let db = Db::connect(&config).await.unwrap();
	let base = &VTXO_VECTORS.board_vtxo;
	let key = base.user_pubkey();
	let (node, _) = db.write(async |t| t.register_lightning_node(&key).await).await.unwrap();
	let preimage = Preimage::from([46; 32]);
	let hash = preimage.compute_payment_hash();
	let expiry = BlockHeight::new(1_000);
	let mut ids = Vec::new();
	for index in 0..3 {
		let point = bitcoin::OutPoint::new(base.point().txid, index);
		let policy = if index == 2 {
			VtxoPolicy::new_server_htlc_recv(key, hash, expiry, BlockDelta::new(40))
		} else { VtxoPolicy::new_server_htlc_send(key, hash, expiry) };
		let coin = ServerVtxo::new(point, policy.into(), sat(10_000), base.expiry_height(),
			base.server_pubkey(), base.exit_delta(), point);
		db.write(async |t| {
			t.execute_vtxo_tree_update(VtxoTreeUpdate::new().insert_spendable_bare_vtxos([coin.clone()])
				.bind_owner_mailboxes([(coin.id(), super::test_server_mailbox_key().public_key())])?).await?;
			htlc_vtxo::create_htlc_vtxos(t, &[(coin.id(), hash, expiry)],
				if index == 2 { HtlcDirection::Outgoing } else { HtlcDirection::Incoming }).await
		}).await.unwrap();
		ids.push(coin.id());
	}
	let invoice_key = SecretKey::from_slice(&[41; 32]).unwrap();
	let secp = Secp256k1::new();
	let bolt11 = InvoiceBuilder::new(Currency::Regtest)
		.description("settlement source association fixture".into()).payment_hash(hash.to_sha256_hash())
		.payment_secret(PaymentSecret([42; 32])).current_timestamp()
		.min_final_cltv_expiry_delta(18).amount_milli_satoshis(10_000_000)
		.build_signed(|message| secp.sign_ecdsa_recoverable(message, &invoice_key)).unwrap();
	let mailbox = ark::mailbox::MailboxIdentifier::from_pubkey(super::test_server_mailbox_key().public_key());
	let subscription = db.write(async |t| {
		t.store_generated_lightning_receive(node, &bolt11, 10_000_000, Some(&mailbox), Some(key), None).await?;
		Ok(t.get_htlc_subscription_by_payment_hash(hash).await?.unwrap().id)
	}).await.unwrap();
	db.write(async |t| t.store_lightning_htlc_subscription_status(subscription,
		LightningHtlcSubscriptionStatus::Accepted, Some(expiry), None).await).await.unwrap();
	db.write(async |t| t.update_lightning_htlc_subscription_with_htlcs(subscription, [ids[2]]).await).await.unwrap();
	let invoice = Invoice::Bolt11(bolt11);
	for (index, source) in ids[..2].iter().enumerate() {
		legacy_payment_attempt(&db, node, &invoice, &[*source]).await;
		if index == 0 {
			let attempt = db.read(async |t| t.get_latest_payment_attempt_by_payment_hash(hash).await).await.unwrap().unwrap();
			assert!(db.write(async |t| t.update_lightning_payment_attempt_status(&attempt,
				server::database::ln::LightningPaymentStatus::Failed, None).await).await.unwrap().is_some());
		}
	}
	let attempts = db.read(async |t| Ok(t.query("SELECT id FROM lightning_payment_attempt WHERE payment_hash=$1 ORDER BY id",
		&[&hash.to_string()]).await?.into_iter().map(|r| r.get::<_, i64>(0)).collect::<Vec<_>>())).await.unwrap();
	assert_eq!(attempts.len(), 2);
	let predicate = async |source| db.read(async |t| t.source_settlement(hash, source).await).await.unwrap();
	assert_eq!(predicate(ids[0]).await, SourceSettlement::NotSettled);
	assert!(db.write(async |t| t.store_htlc_settlement(preimage).await).await.unwrap().is_some());
	for id in &ids { assert_eq!(predicate(*id).await, SourceSettlement::Unknown); }
	// A same-hash preimage cannot discharge the failed or unattempted source.
	// An exact positive source fact can, but never without the matching WAL.
	db.write(async |t| Ok(t.execute("UPDATE lightning_payment_attempt SET status='succeeded',updated_at=NOW() WHERE id=$1",
		&[&attempts[0]]).await?)).await.unwrap();
	assert_eq!(predicate(ids[0]).await, SourceSettlement::Funded);
	assert_eq!(predicate(ids[1]).await, SourceSettlement::Unknown);
	// Once the other source's success accounts for the invoice, a terminal
	// failed source can be returned to its owner. An open attempt cannot.
	db.write(async |t| Ok(t.execute("UPDATE lightning_payment_attempt SET status='failed',updated_at=NOW() WHERE id=$1",
		&[&attempts[1]]).await?)).await.unwrap();
	assert_eq!(predicate(ids[1]).await, SourceSettlement::NotSettled);
	let without_wal: anyhow::Result<()> = db.write(async |t| {
		t.execute("DELETE FROM htlc_settlement WHERE payment_hash=$1", &[&hash.to_string()]).await?;
		assert_eq!(t.source_settlement(hash, ids[0]).await?, SourceSettlement::Unknown);
		let attempt = t.get_lightning_payment_attempt_by_id(attempts[0]).await?.unwrap();
		assert!(t.ensure_attempt_sources_allow_failure(&attempt).await.is_err());
		anyhow::bail!("roll back missing-WAL metadata control")
	}).await;
	assert!(without_wal.is_err());
	assert_eq!(predicate(ids[0]).await, SourceSettlement::Funded);
	db.write(async |t| t.store_lightning_htlc_subscription_status(subscription,
		LightningHtlcSubscriptionStatus::Settled, None, None).await).await.unwrap();
	assert_eq!(predicate(ids[2]).await, SourceSettlement::Funded);
	assert_eq!(predicate(ids[1]).await, SourceSettlement::NotSettled);
	assert_eq!(db.write(async |t| t.store_htlc_settlement(preimage).await).await.unwrap(), None);
	assert_eq!(db.read(async |t| Ok(t.query_one("SELECT count(*) FROM htlc_settlement WHERE payment_hash=$1",
		&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 1);
}
