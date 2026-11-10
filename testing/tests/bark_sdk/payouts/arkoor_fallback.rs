/// A recipient without a server mailbox cannot receive a payable coin.
#[tokio::test]
async fn send_to_address_without_server_mailbox_is_refused() {
	let ctx = TestContext::new("bark_sdk/send_without_server_mailbox_refused").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let sender = ctx.bark_sdk("sender", &srv).boarded(sat(400_000)).create().await;
	let receiver = ctx.bark_sdk("recipient", &srv).create().await;
	let address = receiver.new_address().await.unwrap();
	let before = sender.peek_next_keypair().await.unwrap().1;
	let no_mailbox = ark::Address::new(address.is_testnet(), address.ark_id(), address.policy().clone(), vec![]);
	let error = sender.send_arkoor_payment(&no_mailbox, sat(100_000)).await.unwrap_err();
	assert_eq!(bark::FallbackRefusal::from_error(&error), Some(bark::FallbackRefusal::RecipientMustUpdate));
	assert_eq!(sender.peek_next_keypair().await.unwrap().1, before);
	assert!(sender.pending_arkoor_sends().await.unwrap().is_empty());
	assert_eq!(assert_balance_consistent(&sender, false).await.spendable, sat(400_000));
}

#[tokio::test]
async fn offline_addresses_receive_split_and_isolated_outputs() {
	let ctx = TestContext::new("bark_sdk/offline_addresses_split_isolated").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let sender = ctx.bark_sdk("sender", &srv).boarded(sat(400_000))
		.cfg(|cfg| { cfg.daemon_manual_sync = true; cfg.change_vtxo_split_factor = 3; }).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	sender.stop_daemon_wait().await.unwrap();
	recipient.stop_daemon_wait().await.unwrap();
	let recipient_mailbox = recipient.mailbox_identifier();
	let sender_mnemonic = tokio::fs::read_to_string(ctx.datadir.join("sender/mnemonic"))
		.await.unwrap().trim().parse::<bip39::Mnemonic>().unwrap();
	let recipient_mnemonic = tokio::fs::read_to_string(ctx.datadir.join("recipient/mnemonic"))
		.await.unwrap().trim().parse::<bip39::Mnemonic>().unwrap();
	srv.stop().await.unwrap();
	let first = recipient.new_address().await.unwrap();
	let second = recipient.new_address().await.unwrap();
	assert!(first.delivery().iter().all(|delivery| matches!(delivery, VtxoDelivery::ServerMailbox { .. })));
	drop(recipient);
	drop(sender);
	srv.start().await.unwrap();
	// The fixture restart reserves a new RPC port. Reopen only the sender
	// against that endpoint, preserving its seed, keys and registered script.
	let sender = ctx.bark_sdk("sender", &srv).mnemonic(sender_mnemonic).cfg(|cfg| {
		cfg.daemon_manual_sync = true;
		cfg.change_vtxo_split_factor = 3;
	}).create().await;
	sender.stop_daemon_wait().await.unwrap();
	// The first payment makes three 100,000-sat change outputs; the second isolates change of 1 sat.
	sender.send_arkoor_payment(&first, sat(100_000)).await.unwrap();
	sender.send_arkoor_payment(&second, sat(99_999)).await.unwrap();
	assert_eq!(assert_balance_consistent(&sender, false).await.spendable, sat(200_001));
	assert!(sender.spendable_vtxos().await.unwrap().iter().any(|coin| coin.vtxo.amount() == sat(1)));
	let reopened = ctx.bark_sdk("recipient", &srv).mnemonic(recipient_mnemonic)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	reopened.sync_mailbox().await.unwrap();
	assert_eq!(assert_balance_consistent(&reopened, false).await.spendable, sat(199_999));
	let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
	let received = reopened.spendable_vtxos().await.unwrap();
	for address in [first, second] {
		let coin = received.iter().find(|coin| coin.vtxo.user_pubkey() == address.policy().user_pubkey()).unwrap();
		let owner = db.read(async |tx| tx.coin_owner_mailboxes(&[coin.vtxo.id()]).await).await.unwrap();
		assert_eq!(owner.get(&coin.vtxo.id()), Some(&recipient_mailbox.as_pubkey()));
	}
}

/// An accepted cosign can outlive its reply and the client process. The SDK
/// resumes its persisted action with the same payout destinations, keeping the same
/// economic transaction and the original input locked until finalization.
#[tokio::test]
async fn accepted_arkoor_reply_loss_reopens_and_resigns_the_same_payment() {
	let ctx = TestContext::new("bark_sdk/accepted_arkoor_reply_loss_reopen").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let address = recipient.new_address().await.unwrap();
	#[derive(Clone)]
	struct LoseCosignReplies {
		lose: Arc<AtomicBool>,
		accepted: Arc<Notify>,
		requests: Arc<Mutex<Vec<protos::ArkoorPackageCosignRequest>>>,
	}
	#[async_trait::async_trait]
	impl captaind::proxy::ArkRpcProxy for LoseCosignReplies {
		async fn request_arkoor_cosign(&self, rpc: &mut ArkClient, request: protos::ArkoorPackageCosignRequest)
			-> Result<protos::ArkoorPackageCosignResponse, tonic::Status> {
			let response = rpc.request_arkoor_cosign(request.clone()).await?.into_inner();
			self.requests.lock().unwrap().push(request);
			if self.lose.load(Ordering::SeqCst) {
				self.accepted.notify_one();
				return Err(tonic::Status::unavailable("accepted arkoor reply lost"));
			}
			Ok(response)
		}
	}
	let state = LoseCosignReplies { lose: Arc::new(AtomicBool::new(true)), accepted: Arc::new(Notify::new()), requests: Arc::new(Mutex::new(Vec::new())) };
	let proxy = srv.start_proxy_no_mailbox(state.clone()).await;
	let actor = ctx.bark("sender", &proxy.address).funded(sat(500_000)).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let ids = actor.board_and_confirm_and_register(&ctx, sat(400_000)).await;
	assert_eq!(ids.len(), 1);
	let sender = actor.client().await;
	let source = sender.get_full_vtxo(ids[0]).await.unwrap();
	{
		let send = sender.send_arkoor_payment(&address, sat(100_000));
		tokio::pin!(send);
		tokio::time::timeout(Duration::from_secs(30), async {
			tokio::select! {
				_ = state.accepted.notified() => {},
				result = &mut send => panic!("send completed before its accepted reply was lost: {result:?}"),
			}
		}).await.unwrap();
	}
	let pending = sender.pending_arkoor_sends().await.unwrap();
	assert_eq!(pending.len(), 1);
	assert!(matches!(pending[0].progress, bark::actions::arkoor_send::Progress::Cosigning));
	assert!(matches!(sender.get_vtxo_by_id(ids[0]).await.unwrap().state, bark::vtxo::VtxoState::Locked { .. }));
	assert_eq!(sender.balance().await.unwrap().spendable, sat(0));
	assert_eq!(sender.balance().await.unwrap().total(), sat(400_000));
	let next_key = sender.peek_next_keypair().await.unwrap().1;
	let postgres = srv.config().postgres.clone();
	let db = server::database::Db::connect(&postgres).await.unwrap();
	let economic_rows = async || db.read(async |tx| Ok(tx.query("SELECT vtxo_id,amount,policy,oor_spent_txid \
		FROM vtxo WHERE anchor_point=$1 ORDER BY vtxo_id", &[&source.chain_anchor().to_string()]).await?
		.into_iter().map(|row| (row.get::<_, String>(0), row.get::<_, i64>(1), row.get::<_, Vec<u8>>(2), row.get::<_, Option<String>>(3))).collect::<Vec<_>>())).await.unwrap();
	let accepted_rows = economic_rows().await;
	assert!(accepted_rows.len() > 1, "upstream must already have persisted the actual output transaction");
	let spent_in = accepted_rows.iter().find(|row| row.0 == ids[0].to_string()).unwrap().3.clone();
	assert!(spent_in.is_some());
	drop(sender);
	let reopened = actor.client().await;
	assert_eq!(reopened.pending_arkoor_sends().await.unwrap(), pending, "the actual checkpoint must reconstruct after client reopen");
	assert_eq!(reopened.peek_next_keypair().await.unwrap().1, next_key);
	assert!(matches!(reopened.get_vtxo_by_id(ids[0]).await.unwrap().state, bark::vtxo::VtxoState::Locked { .. }));
	assert_eq!(reopened.balance().await.unwrap().spendable, sat(0), "an accepted source must never reopen as available");
	state.lose.store(false, Ordering::SeqCst);
	reopened.sync_pending_arkoor_sends().await.unwrap();
	assert!(reopened.pending_arkoor_sends().await.unwrap().is_empty());
	assert_eq!(reopened.get_vtxo_by_id(ids[0]).await.unwrap().state, bark::vtxo::VtxoState::Spent);
	assert_eq!(reopened.peek_next_keypair().await.unwrap().1, next_key);
	assert_eq!(economic_rows().await, accepted_rows, "re-signing must preserve every output id, amount, policy and input spend txid");
	let requests = state.requests.lock().unwrap().clone();
	assert!(requests.len() >= 2, "the reopened client must send the accepted transaction again");
	let first = requests.first().unwrap(); let replay = requests.last().unwrap();
	assert_eq!(first.parts.len(), replay.parts.len());
	let own_mailbox = reopened.properties().await.unwrap().mailbox_identity.unwrap().public_key;
	let own_mailbox_bytes = own_mailbox.serialize();
	let mut reconstructed_own_destination = false;
	let recipient_address = address.to_string();
	for (a, b) in first.parts.iter().zip(&replay.parts) {
		assert_eq!(a.input_vtxo_id, b.input_vtxo_id); assert_eq!(a.use_checkpoint, b.use_checkpoint);
		for (old, new) in [(&a.outputs, &b.outputs), (&a.isolated_outputs, &b.isolated_outputs)] {
			assert_eq!(old.len(), new.len());
			for (old, new) in old.iter().zip(new) {
				assert_eq!(old.total_amount, new.total_amount); assert_eq!(old.policy, new.policy);
				assert_eq!(old.address, new.address);
				assert_eq!(old.mailbox_id, new.mailbox_id);
				let key = VtxoPolicy::deserialize(&old.policy).unwrap().user_pubkey();
				if key == address.policy().user_pubkey() {
					assert_eq!(new.address.as_deref(), Some(recipient_address.as_str()),
						"copied recipient address remains authoritative");
					assert!(new.mailbox_id.is_none());
				} else {
					assert_eq!(new.mailbox_id.as_deref(), Some(own_mailbox_bytes.as_slice()));
					assert!(new.address.is_none());
					reconstructed_own_destination = true;
				}
			}
		}
	}
	assert!(reconstructed_own_destination, "the resumed SDK must retain its own payout destination");
	recipient.sync_mailbox().await.unwrap();
	assert_eq!(assert_balance_consistent(&reopened, false).await.spendable, sat(300_000));
	assert_eq!(assert_balance_consistent(&recipient, false).await.spendable, sat(100_000));
	let sends = reopened.history().await.unwrap().into_iter().filter(|entry| entry.subsystem.name == "bark.arkoor" && entry.subsystem.kind == "send").collect::<Vec<_>>();
	assert_eq!(sends.len(), 1); assert_eq!(sends[0].status, MovementStatus::Successful);
	assert_eq!(sends[0].sent_to.len(), 1); assert_eq!(sends[0].sent_to[0].amount, sat(100_000));
	let history = recipient.history().await.unwrap();
	assert_eq!(history.iter().filter(|entry| entry.subsystem.name == "bark.arkoor" && entry.subsystem.kind == "receive").count(), 1);
	reopened.sync_pending_arkoor_sends().await.unwrap(); recipient.sync_mailbox().await.unwrap();
	assert_eq!(recipient.history().await.unwrap(), history, "recovery must not deliver a second payment");
}
