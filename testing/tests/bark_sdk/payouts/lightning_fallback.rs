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


/// Refuses every receive claim as decided for fallback and reports a
/// hash-level `Spent` outcome. With `foreign_owner` set, coin status names
/// another mailbox as the owner.
#[derive(Clone)]
struct DecidedReceive {
	foreign_owner: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for DecidedReceive {
	async fn claim_lightning_receive(
		&self, _upstream: &mut ArkClient, _request: protos::ClaimLightningReceiveRequest,
	) -> Result<protos::ArkoorPackageCosignResponse, tonic::Status> {
		let mut refusal = tonic::Status::invalid_argument("receive grant is decided for fallback");
		refusal.metadata_mut().insert("abandon-reason", "decided_for_fallback".parse().unwrap());
		Err(refusal)
	}

	async fn check_lightning_receive(
		&self, upstream: &mut ArkClient, request: protos::CheckLightningReceiveRequest,
	) -> Result<protos::CheckLightningReceiveResponse, tonic::Status> {
		let mut response = upstream.check_lightning_receive(request).await?.into_inner();
		response.fallback_spend_state = Some(protos::VtxoSpendState::Spent as i32);
		Ok(response)
	}

	async fn get_vtxo_status(
		&self, upstream: &mut ArkClient, request: protos::GetVtxoStatusRequest,
	) -> Result<protos::GetVtxoStatusResponse, tonic::Status> {
		let mut response = upstream.get_vtxo_status(request).await?.into_inner();
		if self.foreign_owner.load(Ordering::SeqCst) {
			let other = bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[7; 32]).unwrap();
			response.owner_mailbox = other.public_key().serialize().to_vec();
		}
		Ok(response)
	}
}

/// Pay a new receive whose claim the proxy refuses, then drive it as far as it goes.
async fn decided_receive(
	srv: &Captaind, lightning: &Arc<LightningPaymentSetup>, wallet: &Wallet, destination: Option<ark::Address>,
) -> (PaymentHash, anyhow::Result<LightningReceiveState>) {
	let invoice = wallet.bolt11_invoice_builder().amount(sat(50_000)).claim_destination(destination)
		.build().await.unwrap();
	let hash = PaymentHash::from(&invoice);
	let (payer, bolt11) = (lightning.clone(), invoice.to_string());
	// The refused claim never settles the hold invoice, so this payment stays in flight.
	tokio::spawn(async move { payer.external.try_pay_bolt11(bolt11).await });
	srv.wait_for_lightning_receive_accepted(hash).await;
	(hash, wallet.try_claim_lightning_receive(hash, true).await)
}

/// The receive stays parked after disclosing its preimage: its movement is
/// unfinished, nothing is settled, and its grants have no payout status.
async fn assert_parked(wallet: &Wallet, hash: PaymentHash, result: anyhow::Result<LightningReceiveState>)
	-> (MovementId, Vec<ark::VtxoId>)
{
	let error = result.expect_err("a hash-level outcome finished the receive");
	assert_eq!(bark::FallbackRefusal::from_error(&error), Some(bark::FallbackRefusal::DecidedForFallback), "{error:#}");
	let LightningReceiveState::InProgress(receive) = wallet.lightning_receive_state(hash).await.unwrap() else {
		panic!("a hash-level outcome settled the receive");
	};
	let Progress::PreimageRevealed(htlcs) = receive.progress else { panic!("unexpected {:?}", receive.progress) };
	let movement = wallet.history().await.unwrap().into_iter().find(|m| m.id == htlcs.movement_id).unwrap();
	assert_eq!(movement.status, MovementStatus::Pending);
	for id in &htlcs.vtxo_ids {
		assert_eq!(wallet.get_vtxo_by_id(*id).await.unwrap().fallback_status, None);
	}
	(htlcs.movement_id, htlcs.vtxo_ids)
}

/// A hash-level fallback outcome is not a delivery receipt. It finishes a
/// local receive only after every grant's authenticated status names this
/// wallet's mailbox, and never finishes a receive forwarded to another wallet.
#[tokio::test]
async fn hash_fallback_outcome_finishes_only_an_owned_local_receive() {
	let ctx = TestContext::new("bark_sdk/hash_fallback_outcome_owned_local_receive").await;
	let lightning = Arc::new(ctx.new_lightning_setup("lightningd").await);
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.expiry_payouts = true).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let gate = DecidedReceive { foreign_owner: Arc::new(AtomicBool::new(false)) };
	let proxy = srv.start_proxy_no_mailbox(gate.clone()).await;
	let wallet = ctx.bark_sdk("receiver", &proxy).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv).create().await;
	lightning.sync().await;

	gate.foreign_owner.store(true, Ordering::SeqCst);
	let (local, result) = decided_receive(&srv, &lightning, &wallet, None).await;
	let (movement, grants) = assert_parked(&wallet, local, result).await;
	// Once the grants are attested as this wallet's, the same outcome finishes it.
	gate.foreign_owner.store(false, Ordering::SeqCst);
	assert!(matches!(wallet.try_claim_lightning_receive(local, true).await.unwrap(), LightningReceiveState::Settled(_)));
	let finished = wallet.history().await.unwrap().into_iter().find(|m| m.id == movement).unwrap();
	assert_eq!((finished.status, finished.effective_balance), (MovementStatus::Successful, bitcoin::SignedAmount::ZERO));
	for id in grants {
		assert_eq!(wallet.get_vtxo_by_id(id).await.unwrap().fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
	}

	// The outcome that finished the owned receive cannot finish a forwarded one.
	let destination = recipient.new_address().await.unwrap();
	let (forwarded, result) = decided_receive(&srv, &lightning, &wallet, Some(destination)).await;
	assert_parked(&wallet, forwarded, result).await;
}
