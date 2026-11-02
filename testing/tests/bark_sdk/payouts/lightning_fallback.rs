#[tokio::test]
async fn pay_hold_expired_inputs_are_refused_then_refresh_succeeds() {
	tokio::time::timeout(Duration::from_secs(180), Box::pin(async {
		async fn assert_not_dispatched(
			db: &Db, node: &ark_testing::daemon::lightningd::Lightningd,
			hold_client: &mut hold::hold_client::HoldClient<tonic::transport::Channel>, hash: PaymentHash,
		) {
			let rows = db.read(async |tx| Ok(tx.query_one("SELECT \
				(SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1) AS attempts", &[&hash.to_string()]).await?)).await.unwrap();
			assert_eq!(rows.get::<_, i64>("attempts"), 0);
			let pays = node.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
				bolt11: None, payment_hash: Some(hash.to_vec()), status: None,
				index: None, limit: None, start: None,
			}).await.unwrap().into_inner().pays;
			assert!(pays.is_empty(), "a refused grant must not reach the real payer");
			let invoices = hold_client.list(hold::ListRequest {
				constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
			}).await.unwrap().into_inner().invoices;
			assert_eq!(invoices.len(), 1);
			assert_eq!(invoices[0].state(), hold::InvoiceState::Unpaid);
			assert!(invoices[0].htlcs.is_empty());
		}

		let ctx = TestContext::new("bark_sdk/pay_hold_expired_refusal_then_refresh").await;
		let lightning = Box::pin(ctx.new_lightning_setup("lightningd")).await;
		// Isolate new-grant admission before a real fallback decision. These
		// explicit scheduling settings do not qualify default-daemon behavior.
		let srv = Box::pin(ctx.captaind("server").lightningd(&lightning.internal).cfg(|cfg| { cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(100);
			cfg.round_interval = Duration::from_secs(86400);
		}).watchmand_cfg(|cfg| {
			cfg.watchman.reaction_interval = Duration::from_secs(900);
			cfg.watchman.sweep_interval = Duration::from_secs(900);
		}).funded(btc(10)).create()).await;
		let board_amount = btc(2);
		let wallet = Box::pin(ctx.bark_sdk("bark", &srv).cfg(|cfg| cfg.daemon_manual_sync = true)
			.boarded(board_amount).create()).await;
		let db = Db::connect(&srv.config().postgres).await.unwrap();
		let original = wallet.vtxos().await.unwrap();
		assert_eq!(original.len(), 1);
		let source = wallet.get_full_vtxo(original[0].id()).await.unwrap();
		assert_eq!(source.amount(), board_amount);
		let source_body = ark::ProtocolEncoding::serialize(&source);
		let source_anchor = ctx.bitcoind().await_transaction(source.chain_anchor().txid).await;
		source.validate(&source_anchor).unwrap();
		let tip = ctx.bitcoind().get_block_count().await as u32;
		assert!(tip < source.expiry_height().to_u32());
		wallet.fallback_script();
		let earliest = db.read(async |tx| Ok(tx.query_one(
			"SELECT MIN(expiry) FROM vtxo WHERE frontier_at IS NOT NULL AND onchain_spent_height IS NULL", &[],
		).await?.get::<_, Option<i32>>(0))).await.unwrap().expect("fixture must have a real backing frontier");
		assert!(i64::from(earliest) > i64::from(tip), "the watchman pause must precede every backing maturity");
		// Pause scheduled fallback while ordinary registration and the real
		// refresh remain available through the running server.
		srv.watchmand().stop().await.unwrap();
		let expired = ctx.generate_blocks(source.expiry_height().to_u32() - tip).await;
		srv.wait_for_sync_height(expired).await;
		wallet.chain().invalidate_caches().await;
		assert_eq!(wallet.chain().tip().await.unwrap(), source.expiry_height());
		let native = db.read(async |tx| tx.get_user_vtxo_by_id(source.id()).await).await.unwrap();
		assert_eq!(native.spend_state, server::database::SpendState::Spendable);
		assert!(!native.decided_for_fallback, "the fixture must reach expiry before any native payout decision");
		assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1",
			&[&source.id().to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
		lightning.sync().await;

		let invoice_amount = btc(0.5);
		let refused_hash = Preimage::random().compute_payment_hash();
		let mut hold_client = lightning.external.hold_client().await;
		let refused_invoice = hold_client.invoice(hold::InvoiceRequest {
			payment_hash: refused_hash.to_vec(), amount_msat: invoice_amount.to_sat() * 1_000,
			description: Some(hold::invoice_request::Description::Memo("expired-new-grant-refusal".into())),
			min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
		}).await.unwrap().into_inner().bolt11.parse::<ark::lightning::Invoice>().unwrap();
		refused_invoice.check_signature().unwrap();
		assert_eq!(refused_invoice.network(), bitcoin::Network::Regtest);
		assert_eq!(refused_invoice.payment_hash(), refused_hash);
		assert_eq!(refused_invoice.amount_msat(), Some(invoice_amount.to_sat() * 1_000));
		let previous_history = wallet.history().await.unwrap();
		let error = wallet.pay_lightning_invoice(refused_invoice, None, false).await.unwrap_err();
		let refusal = bark::FallbackRefusal::from_error(&error).expect("typed expired-new-grant refusal");
		assert_eq!(refusal, bark::FallbackRefusal::ExpiredNewLightning);
		assert_eq!(refusal.code(), "expired_new_lightning");
		assert_eq!(ark::ProtocolEncoding::serialize(&wallet.get_full_vtxo(source.id()).await.unwrap()), source_body);
		let retained = wallet.vtxos().await.unwrap();
		assert_eq!(retained.len(), 1);
		assert_eq!(retained[0].id(), source.id());
		assert_eq!(retained[0].state, bark::vtxo::VtxoState::Spendable);
		let balance = assert_balance_consistent(&wallet, false).await;
		assert_eq!(balance.total(), board_amount);
		assert_eq!(balance.needs_refresh, board_amount);
		assert_eq!(balance.pending(), bitcoin::Amount::ZERO);
		assert!(wallet.pending_lightning_sends().await.unwrap().is_empty());
		let history = wallet.history().await.unwrap();
		let failed = history.iter().filter(|movement|
			!previous_history.iter().any(|old| old.id == movement.id)).collect::<Vec<_>>();
		assert_eq!(failed.len(), 1);
		assert_eq!(failed[0].status, MovementStatus::Failed);
		assert_eq!(failed[0].effective_balance.to_sat(), 0);
		assert_eq!(failed[0].input_vtxos, vec![source.id()]);
		let native = db.read(async |tx| tx.get_user_vtxo_by_id(source.id()).await).await.unwrap();
		assert_eq!(native.spend_state, server::database::SpendState::Spendable);
		assert!(!native.decided_for_fallback);
		assert_not_dispatched(&db, &lightning.internal, &mut hold_client, refused_hash).await;

		// A round replaces the expired source; no expiry is extended
		// and no financial state is injected by the fixture.
		let (refresh, _) = tokio::join!(wallet.refresh_vtxos([source.id()]), async {
			tokio::time::sleep(Duration::from_secs(2)).await;
			srv.trigger_round().await;
		});
		let refresh = refresh.unwrap().expect("the expired source must participate in a real refresh");
		assert!(matches!(refresh, bark::round::RoundStatus::Unconfirmed { .. }));
		let committed = db.read(async |tx| tx.get_user_vtxo_by_id(source.id()).await).await.unwrap();
		assert_eq!(committed.spend_state, server::database::SpendState::Spent);
		assert!(committed.spent_in_round.is_some(), "the real refresh must commit its source binding");
		assert!(!committed.decided_for_fallback);
		let confirmed = ctx.generate_blocks(ark_testing::constants::ROUND_CONFIRMATIONS).await;
		wallet.chain().invalidate_caches().await;
		tokio::time::timeout(Duration::from_secs(15), async {
			loop {
				wallet.sync().await;
				if wallet.balance().await.unwrap().pending_in_round == bitcoin::Amount::ZERO { break; }
				tokio::time::sleep(poll_interval()).await;
			}
		}).await.expect("the real refreshed round must confirm");
		let replacement = wallet.vtxos().await.unwrap();
		assert_eq!(replacement.len(), 1);
		assert_ne!(replacement[0].id(), source.id());
		assert_eq!(replacement[0].amount(), board_amount, "the declared refresh fee is zero");
		let replacement = wallet.get_full_vtxo(replacement[0].id()).await.unwrap();
		assert_ne!(replacement.chain_anchor(), source.chain_anchor());
		assert!(replacement.expiry_height() > wallet.chain().tip().await.unwrap());
		let anchor_info = bitcoin_ext::rpc::BitcoinRpcExt::custom_get_raw_transaction_info(
			&ctx.bitcoind().sync_client(), replacement.chain_anchor().txid, None,
		).unwrap().unwrap();
		assert!(anchor_info.confirmations.unwrap_or(0) >= 6);
		let anchor = ctx.bitcoind().await_transaction(replacement.chain_anchor().txid).await;
		replacement.validate(&anchor).unwrap();
		let native = db.read(async |tx| tx.get_user_vtxo_by_id(source.id()).await).await.unwrap();
		assert_eq!(native.spend_state, server::database::SpendState::Spent);
		assert!(native.spent_in_round.is_some());
		let forfeits = db.read(async |tx| Ok(tx.query(
			"SELECT p.id,r.funding_txid,i.signed_forfeit_tx IS NOT NULL AS signed,p.forfeited_at IS NOT NULL AS accepted \
			FROM round_part_input i JOIN round_participation p ON p.id=i.participation_id \
			JOIN round r ON r.funding_txid=p.round_id WHERE i.vtxo_id=$1 AND r.id=$2",
			&[&source.id().to_string(), &native.spent_in_round.unwrap()],
		).await?)).await.unwrap();
		assert_eq!(forfeits.len(), 1, "the old source must fund its actual accepted refresh");
		let forfeit = &forfeits[0];
		assert!(forfeit.get::<_, bool>("signed") && forfeit.get::<_, bool>("accepted"));
		assert_eq!(forfeit.get::<_, String>("funding_txid"), replacement.chain_anchor().txid.to_string());
		// Issuance alone can still be canceled; resume fallback only after
		// the real confirmed swap has accepted the old source's forfeit.
		srv.watchmand().start().await.unwrap();
		srv.watchmand().wait_for_sync_height(confirmed).await;
		// Once the watchman resumes, expiry may decide the surrendered old
		// backing for the operator. It must never pay that principal again.
		let allocations = db.read(async |tx| Ok(tx.query(
			"SELECT (SELECT amount FROM vtxo original WHERE original.vtxo_id=e.vtxo_id) AS gross_sat,(SELECT evidence::TEXT FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1) AS facts,c.gross_sat AS allocated_gross_sat,c.disposition,c.owner_mailbox,c.user_pubkey,c.spk, \
			c.fee_sat,c.payout_sat,c.forfeited_sat FROM expiry_entitlement e \
			LEFT JOIN expiry_allocation_coin c ON c.entitlement_id=e.id WHERE e.vtxo_id=$1",
			&[&source.id().to_string()],
		).await?)).await.unwrap();
		if native.decided_for_fallback { assert!(!allocations.is_empty()); }
		for allocation in allocations {
			assert_eq!(allocation.get::<_, i64>("gross_sat"), i64::try_from(board_amount.to_sat()).unwrap());
			let facts: serde_json::Value = serde_json::from_str(allocation.get("facts")).unwrap();
			assert_eq!(facts["kind"], "exchange");


			if let Some(disposition) = allocation.get::<_, Option<String>>("disposition") {
				assert_eq!(disposition, "operator");
				assert_eq!(allocation.get::<_, i64>("allocated_gross_sat"), i64::try_from(board_amount.to_sat()).unwrap());
				for field in ["owner_mailbox", "user_pubkey", "spk"] {
					assert!(allocation.get::<_, Option<String>>(field).is_none());
				}
				for field in ["fee_sat", "payout_sat", "forfeited_sat"] {
					assert_eq!(allocation.get::<_, i64>(field), 0);
				}
			}
		}
		let native_replacement = db.read(async |tx| tx.get_user_vtxo_by_id(replacement.id()).await).await.unwrap();
		assert_eq!(native_replacement.spend_state, server::database::SpendState::Spendable);
		assert!(!native_replacement.decided_for_fallback);
		assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1",
			&[&replacement.id().to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
		assert!(wallet.history().await.unwrap().iter().all(|movement| movement.subsystem.name != "scuttle.fallback"));
		let balance = assert_balance_consistent(&wallet, false).await;
		assert_eq!(balance.spendable, board_amount);
		assert_eq!(balance.needs_refresh, bitcoin::Amount::ZERO);
		lightning.sync().await;

		let preimage = Preimage::random();
		let payment_hash = preimage.compute_payment_hash();
		assert_ne!(payment_hash, refused_hash);
		let invoice = hold_client.invoice(hold::InvoiceRequest {
			payment_hash: payment_hash.to_vec(), amount_msat: invoice_amount.to_sat() * 1_000,
			description: Some(hold::invoice_request::Description::Memo("refreshed-native-hold-payment".into())),
			min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
		}).await.unwrap().into_inner().bolt11.parse::<ark::lightning::Invoice>().unwrap();
		invoice.check_signature().unwrap();
		assert_eq!(invoice.network(), bitcoin::Network::Regtest);
		assert_eq!(invoice.payment_hash(), payment_hash);
		assert_eq!(invoice.amount_msat(), Some(invoice_amount.to_sat() * 1_000));
		wallet.pay_lightning_invoice(invoice, None, false).await.expect("a genuinely refreshed input must pay");
		wait_for_hold_invoice_accepted(&mut hold_client, payment_hash).await;
		let pending = wallet.pending_lightning_sends().await.unwrap();
		assert_eq!(pending.len(), 1);
		assert_eq!(pending[0].input_vtxo_ids, vec![replacement.id()]);
		assert_eq!(pending[0].invoice.payment_hash(), payment_hash);
		assert_eq!(pending[0].fee, bitcoin::Amount::ZERO, "this fixture declares zero user Lightning fee");
		let status = wallet.check_lightning_payment(payment_hash, false).await.unwrap();
		assert!(matches!(&status, LightningSendState::InProgress(pending) if pending.payment_amount == invoice_amount),
			"real hold invoice should remain pending for the original amount: {status:?}");
		let waiter = {
			let wallet = wallet.clone();
			tokio::spawn(async move { wallet.check_lightning_payment(payment_hash, true).await })
		};
		tokio::time::sleep(Duration::from_millis(500)).await;
		assert!(!waiter.is_finished(), "waiting check resolved before real settlement");
		hold_client.settle(hold::SettleRequest { payment_preimage: preimage.to_vec() }).await.unwrap();
		let status = waiter.wait_millis(10_000).await.unwrap().unwrap();
		let LightningSendState::Paid(paid) = &status else { panic!("real payment did not settle: {status:?}"); };
		assert_eq!(paid.payment_hash, payment_hash);
		assert_eq!(paid.preimage, preimage);
		assert_eq!(wallet.check_lightning_payment(payment_hash, false).await.unwrap(), status);
		let invoices = hold_client.list(hold::ListRequest {
			constraint: Some(hold::list_request::Constraint::PaymentHash(payment_hash.to_vec())),
		}).await.unwrap().into_inner().invoices;
		assert_eq!(invoices.len(), 1);
		assert_eq!(invoices[0].state(), hold::InvoiceState::Paid);
		let pays = lightning.internal.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
			bolt11: None, payment_hash: Some(payment_hash.to_vec()), status: None,
			index: None, limit: None, start: None,
		}).await.unwrap().into_inner().pays;
		assert_eq!(pays.len(), 1);
		assert_eq!(pays[0].status(), cln_rpc::listpays_pays::ListpaysPaysStatus::Complete);
		assert_eq!(pays[0].payment_hash, payment_hash.to_vec());
		assert_eq!(pays[0].preimage.as_deref(), Some(preimage.as_ref()));
		let balance = assert_balance_consistent(&wallet, false).await;
		assert_eq!(balance.spendable, board_amount - invoice_amount);
		assert_eq!(balance.needs_refresh, bitcoin::Amount::ZERO);
		assert_eq!(balance.pending(), bitcoin::Amount::ZERO);
		assert_not_dispatched(&db, &lightning.internal, &mut hold_client, refused_hash).await;
	})).await.expect("expired grant refusal, authentic refresh and hold settlement must finish within180s");
}

#[tokio::test]
async fn receive_claim_reply_loss_is_durable_before_wire_and_recovers_after_reopen() {
	let ctx = TestContext::new("bark_sdk/receive_claim_reply_loss_reopen").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.expiry_payouts = false).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let gate = HoldSuccessfulReceiveClaim { hold: Arc::new(AtomicBool::new(true)),
		arrived: Arc::new(tokio::sync::Notify::new()), requests: Arc::new(AtomicUsize::new(0)),
		hold_before: false, release_before: Arc::new(tokio::sync::Notify::new()) };
	let proxy = srv.start_proxy_no_mailbox(gate.clone()).await;
	let wallet = ctx.bark_sdk("receiver", &proxy).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	lightning.sync().await;
	let amount = btc(0.5);
	let invoice = wallet.bolt11_invoice(amount, None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	let mut mailbox = wallet.subscribe_mailbox_messages(None).await.unwrap();
	let (paid, checkpoint) = tokio::join!(lightning.external.try_pay_bolt11(invoice.to_string()), async {
		loop {
			let message = mailbox.next().wait_millis(10_000).await.unwrap().unwrap();
			if let Some(MailboxMsg::IncomingLightningPayment(message)) = message.message {
				assert_eq!(message.payment_hash, hash.to_vec());
				break;
			}
		}
		// One attempt: a pre-wire error must end the test path, rather than
		// retrying from a parked state and accidentally concealing this window.
		let mut claim = Box::pin(wallet.try_claim_lightning_receive(hash, false));
		tokio::select! {
			result = &mut claim => panic!("claim completed before its actual upstream reply was released: {result:?}"),
			_ = async { tokio::time::timeout(Duration::from_secs(30), gate.arrived.notified()).await.unwrap(); } => {},
		}
		let checkpoint = wallet.lightning_receive_checkpoint(hash).await.unwrap().unwrap();
		assert!(futures::poll!(&mut claim).is_pending(),
			"the captured checkpoint must precede any client reply or timeout-error handling");
		assert_eq!(gate.requests.load(Ordering::SeqCst), 1);
		drop(claim);
		checkpoint
	});
	paid.expect("the real Lightning payer should settle from the accepted preimage");
	let mut hold = lightning.internal.hold_client().await;
	let invoices = hold.list(hold::ListRequest {
		constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner().invoices;
	assert_eq!(invoices.first().unwrap().state(), hold::InvoiceState::Paid,
		"the real receiver hold invoice, not an injected RPC failure, must already be settled");
	// Assert only after the external payment and receiver settlement
	// succeed. The snapshot itself was read while the client awaited a reply.
	assert!(matches!(&checkpoint.progress, bark::actions::lightning::receive::Progress::PreimageRevealed(_)),
		"a completed payment with its client reply still lost must already be durably irreversible");
	drop(mailbox);
	wallet.stop_daemon_wait().await.unwrap();
	drop(wallet);
	// Use only the original private seed and original app/Ark databases.
	// The controller never injects a preimage, receipt, or action state.
	let mnemonic = bip39::Mnemonic::from_str(tokio::fs::read_to_string(ctx.datadir.join("receiver/mnemonic"))
		.await.unwrap().trim()).unwrap();
	let database = Arc::new(SqliteClient::open(ctx.datadir.join("receiver/db.sqlite")).unwrap());
	let onchain: Arc<tokio::sync::RwLock<dyn OnchainWalletTrait>> = Arc::new(tokio::sync::RwLock::new(
		OnchainWallet::load_or_create(Network::Regtest, mnemonic.to_seed(""), database.clone()).await.unwrap(),
	));
	let mut config = ctx.bark_default_cfg(&proxy, Some(ctx.bitcoind()));
	config.daemon_manual_sync = true;
	let reopened = Wallet::open(Network::Regtest, WalletSeed::new_from_mnemonic(Network::Regtest, &mnemonic), config,
		OpenWalletArgs { persister: Some(database), onchain: Some(onchain), run_daemon: false, ..Default::default() }).await.unwrap();
	assert_eq!(reopened.lightning_receive_checkpoint(hash).await.unwrap().unwrap(), checkpoint);
	assert!(matches!(reopened.try_claim_lightning_receive(hash, true).await.unwrap(), LightningReceiveState::Settled(_)));
	assert!(gate.requests.load(Ordering::SeqCst) >= 2, "reopen must execute the genuine idempotent claim RPC again");
	assert!(reopened.lightning_receive_checkpoint(hash).await.unwrap().is_none());
	for _ in 0..2 {
		assert!(matches!(reopened.try_claim_lightning_receive(hash, true).await.unwrap(), LightningReceiveState::Settled(_)));
		let balance = assert_balance_consistent(&reopened, false).await;
		assert_eq!(balance.spendable, amount);
		assert_eq!(balance.claimable_lightning_receive, bitcoin::Amount::ZERO);
		let receives = reopened.history().await.unwrap().into_iter().filter(|entry|
			entry.subsystem.is_subsystem(Subsystem::LIGHTNING_RECEIVE)).collect::<Vec<_>>();
		assert_eq!(receives.len(), 1, "lost replies and restart must credit the Lightning receive once");
		assert_eq!(receives[0].status, MovementStatus::Successful);
		assert_eq!(receives[0].effective_balance, amount.to_signed().unwrap());
	}
}
