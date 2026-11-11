
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
