use ark::ProtocolEncoding;

/// accepted refresh replaces one principal; its operator-owned old
/// backing must not add the old source to the customer's replacement balance.
#[tokio::test]
async fn fallback_balance_completed_refresh_and_operator_backing_conserve_one_principal() {
	let ctx = TestContext::new("bark_sdk/fallback_balance_refresh_operator").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool()
		.cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(48)).watchmand().create().await;
	let wallet = boarded_wallet(&ctx, &srv).await;
	let original = wallet.spendable_vtxos().await.unwrap();
	assert_eq!(original.len(), 1);
	let (result, _) = tokio::join!(wallet.refresh_vtxos(original.iter()), async {
		tokio::time::sleep(Duration::from_secs(2)).await;
		srv.trigger_round().await;
	});
	result.unwrap().unwrap();
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	let complete = sync_until(&ctx, &wallet, |balance| balance.pending_in_round == Amount::ZERO).await;
	assert_eq!(complete.total(), AMOUNT);
	let local = SqliteClient::open(ctx.datadir.join("bark/db.sqlite")).unwrap();
	assert_eq!(local.get_wallet_vtxo(original[0].id()).await.unwrap().unwrap().state, bark::vtxo::VtxoState::Spent);
	assert_eq!(wallet.spendable_vtxos().await.unwrap().iter().map(|coin| coin.amount()).sum::<Amount>(), AMOUNT);

	let height = ctx.bitcoind().get_block_count().await as u32;
	let mature = ctx.generate_blocks(original[0].expiry_height().to_u32() + 1 - height).await;
	srv.wait_for_sync_height(mature).await;
	srv.watchmand().wait_for_sync_height(mature).await;
	let database = server::database::Db::connect(&srv.config().postgres).await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let operator = database.read(async |tx| Ok(tx.query_opt("SELECT e.id FROM expiry_entitlement e \
				JOIN vtxo v ON v.vtxo_id=e.vtxo_id WHERE v.anchor_point=$1 \
				AND (SELECT disposition FROM expiry_allocation_coin first_coin WHERE first_coin.entitlement_id=e.id ORDER BY allocation_id LIMIT 1)='operator' LIMIT 1", &[&original[0].chain_anchor().to_string()]).await?)).await.unwrap();
			if operator.is_some() { break; }
			tokio::time::sleep(poll_interval()).await;
		}
	}).await.expect("actual old backing did not receive its operator disposition");
	wallet.sync().await;
	let after_operator = wallet.balance().await.unwrap();
	assert_eq!(after_operator.total(), AMOUNT, "old source plus its replacement cannot become two owned principals");
	assert_eq!(local.get_wallet_vtxo(original[0].id()).await.unwrap().unwrap().state, bark::vtxo::VtxoState::Spent);
}


/// A retained ordinary send checkpoint cannot hide a genuine committed exit.
#[tokio::test]
async fn retained_arkoor_checkpoint_keeps_exited_principal_in_balance() {
	let ctx = TestContext::new("bark_sdk/retained_arkoor_checkpoint_exit_balance").await;
	let srv = ctx.captaind("server").create().await;
	#[derive(Clone)]
	struct RefuseCosign(std::sync::Arc<tokio::sync::Notify>);
	#[async_trait::async_trait]
	impl ark_testing::daemon::captaind::proxy::ArkRpcProxy for RefuseCosign {
		async fn request_arkoor_cosign(&self, _upstream: &mut ark_testing::daemon::captaind::ArkClient,
			_request: server_rpc::protos::ArkoorPackageCosignRequest,
		) -> Result<server_rpc::protos::ArkoorPackageCosignResponse, tonic::Status> {
			// A declared pre-effect transport fault: no cosign reaches the server.
			self.0.notify_one();
			Err(tonic::Status::unavailable("exit balance test: cosign not forwarded"))
		}
	}
	let requested = std::sync::Arc::new(tokio::sync::Notify::new());
	let proxy = srv.start_proxy_no_mailbox(RefuseCosign(requested.clone())).await;
	let wallet = ctx.bark_sdk("bark", &proxy.address)
		.cfg(|cfg| cfg.daemon_manual_sync = true)
		.funded(ark_testing::sat(500_000)).boarded(ark_testing::sat(100_000)).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	wallet.stop_daemon_wait().await.unwrap();
	recipient.stop_daemon_wait().await.unwrap();
	let [source] = wallet.spendable_vtxos().await.unwrap().try_into().expect("one original board");
	let original = wallet.get_full_vtxo(source.id()).await.unwrap();
	let address = recipient.new_address().await.unwrap();
	{
		let send = wallet.send_arkoor_payment(&address, ark_testing::sat(60_000));
		tokio::pin!(send);
		tokio::time::timeout(Duration::from_secs(30), async {
			tokio::select! {
				_ = requested.notified() => {},
				result = &mut send => panic!("send ended before its cosign fault: {result:?}"),
			}
		}).await.unwrap();
	}
	let pending = wallet.pending_arkoor_sends().await.unwrap();
	assert_eq!(pending.len(), 1);
	assert_eq!(pending[0].input_vtxo_ids, vec![source.id()]);
	let checkpoint = serde_json::to_vec(&pending).unwrap();
	let retained = wallet.get_vtxo_by_id(source.id()).await.unwrap();
	assert_eq!(retained.state, bark::vtxo::VtxoState::Locked {
		holder: Some(bark::vtxo::VtxoLockHolder::Action { id: pending[0].id.clone() }),
	});
	assert!(retained.fallback_status.is_none());
	assert_eq!(assert_balance_consistent(&wallet, false).await.pending_arkoor_send, ark_testing::sat(100_000));
	wallet.exit_mgr().start_exit_for_entire_wallet().await.unwrap();
	let mut committed = false;
	for _ in 0..10 {
		wallet.sync_onchain().await.unwrap();
		wallet.progress_exits().await.unwrap();
		let exit = wallet.exit_mgr().get_exit_vtxo(source.id()).await.expect("the original exit is tracked");
		if matches!(exit.state(), bark::exit::ExitState::AwaitingDelta(_)) { committed = true; break; }
		assert!(!matches!(exit.state(), bark::exit::ExitState::Claimable(_) | bark::exit::ExitState::Claimed(_)));
		ctx.generate_blocks(1).await;
		wallet.chain().invalidate_caches().await;
	}
	assert!(committed, "the genuine exit did not reach AwaitingDelta");
	let retained = wallet.get_vtxo_by_id(source.id()).await.unwrap();
	assert_eq!(retained.state, bark::vtxo::VtxoState::Exited);
	assert!(retained.fallback_status.is_none());
	assert!(wallet.get_full_vtxo(source.id()).await.unwrap().serialize() == original.serialize());
	assert!(serde_json::to_vec(&wallet.pending_arkoor_sends().await.unwrap()).unwrap() == checkpoint,
		"exit progression must leave the exact ordinary send checkpoint retained");
	let balance = assert_balance_consistent(&wallet, false).await;
	assert_eq!(balance.pending_exit, ark_testing::sat(100_000));
	assert_eq!(balance.pending(), ark_testing::sat(100_000));
	assert_eq!(balance.total(), ark_testing::sat(100_000));
	assert_eq!(balance.pending_arkoor_send, Amount::ZERO);
	assert_eq!(balance.spendable, Amount::ZERO);
	assert_eq!(balance.needs_refresh, Amount::ZERO);
	assert_eq!(balance.summary().total, ark_testing::sat(100_000));
	assert_eq!(wallet.balance().await.unwrap().total(), balance.total());
	assert_eq!(recipient.balance().await.unwrap().total(), Amount::ZERO);
}
