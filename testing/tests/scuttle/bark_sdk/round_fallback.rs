#[tokio::test]
async fn cancelled_issued_refresh_reopens_and_receives_one_fallback_payout() {
	tokio::time::timeout(Duration::from_secs(180), Box::pin(async {
		let ctx = TestContext::new("bark_sdk/cancelled_refresh_payout").await;
		let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().funded(btc(1))
			.cfg(|config| { config.expiry_payouts = true; config.vtxo_lifetime = BlockDelta::new(48); config.vtxo_exit_delta = BlockDelta::new(3); })
			.watchmand().create().await;
		let transport = RefreshTransport::default();
		let IssuedRefresh { wallet, proxy, database, mnemonic, source, state_id, movement, unlock_hash, sweep_at, first_height, .. } =
			Box::pin(issued_refresh(&ctx, &srv, transport.clone(), RoundWriteFault::default(), true)).await;
		let registration = wallet.fallback_script();
		let attestation = RoundParticipationStatusAttestation::new(unlock_hash, &wallet.mailbox_keypair());
		drop(wallet);
		await_refresh_tombstone(&ctx, &srv, unlock_hash, sweep_at, attestation).await;
		let wallet = Box::pin(reopen_refresh(&ctx, &proxy, &mnemonic, database.clone())).await;
		assert_refresh_held(&wallet, &database, state_id, movement, source.id(), RoundFlowKind::AwaitingConfirmations).await;
		let result = wallet.sync_pending_rounds().await.unwrap();
		assert!(matches!(result.get(&state_id), Some(RoundStatus::Canceled)));
		assert!(wallet.pending_round_states().await.unwrap().is_empty());
		assert_eq!(wallet.get_vtxo_by_id(source.id()).await.unwrap().state, VtxoState::Spendable);
		assert_eq!(wallet.spendable_vtxos().await.unwrap().iter().map(|coin| coin.amount()).sum::<Amount>(), source.amount());
		assert_refresh_balance(&wallet, Amount::ZERO, sat(100_000), Amount::ZERO, Amount::ZERO).await;
		let failed = database.get_movement_by_id(movement).await.unwrap();
		assert_eq!(failed.status, MovementStatus::Failed);
		assert_eq!(failed.effective_balance, bitcoin::SignedAmount::ZERO);
		assert_eq!(failed.offchain_fee, Amount::ZERO);
		assert!(failed.output_vtxos.is_empty());
		let history = wallet.history().await.unwrap();
		for _ in 0..2 { assert!(wallet.sync_pending_rounds().await.unwrap().is_empty()); }
		assert_eq!(wallet.history().await.unwrap(), history);
		drop(wallet);
		let wallet = Box::pin(reopen_refresh(&ctx, &proxy, &mnemonic, database.clone())).await;
		assert!(wallet.pending_round_states().await.unwrap().is_empty());
		assert_eq!(wallet.history().await.unwrap(), history);
		assert_eq!(wallet.get_vtxo_by_id(source.id()).await.unwrap().state, VtxoState::Spendable);
		assert_refresh_balance(&wallet, Amount::ZERO, sat(100_000), Amount::ZERO, Amount::ZERO).await;
		drop(wallet);

		// Keep the owner absent while both the original and the cancelled round tree expire.
		// Separate confirmed coins let the watchman also close its unclaimed round tree.
		let fee_address = srv.watchmand().wait_wallet_address().await;
		let mut sponsors = Vec::new();
		for _ in 0..2 {
			let txid = ctx.bitcoind().fund_addr(fee_address.clone(), sat(100_000)).await;
			let tx = ctx.bitcoind().await_transaction(txid).await;
			let outputs = tx.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == fee_address.script_pubkey()
				&& output.value == sat(100_000)).collect::<Vec<_>>();
			assert_eq!(outputs.len(), 1);
			sponsors.push(OutPoint::new(txid, outputs[0].0 as u32));
		}
		ctx.generate_blocks(2).await;
		let parent = async {
			'paid: loop {
				srv.watchmand().trigger_sweep().await;
				let core = ctx.bitcoind().sync_client();
				let mut txids = core.get_raw_mempool().unwrap();
				for height in first_height + 1..=core.get_block_count().unwrap() {
					txids.extend(core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata.iter().map(Transaction::compute_txid));
				}
				for txid in txids {
					let tx = ctx.bitcoind().await_transaction(txid).await;
					if tx.output.iter().any(|output| output.script_pubkey == registration && output.value == sat(99_000)) {
						break 'paid tx;
					}
				}
				tokio::time::sleep(poll_interval()).await;
			}
		}.wait_millis(30_000).await;
		assert!(parent.input.iter().any(|input| input.previous_output == source.chain_anchor()));
		let customer = parent.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == registration).collect::<Vec<_>>();
		assert_eq!(customer.len(), 1);
		assert_eq!(customer[0].1.value, sat(99_000));
		let payout = OutPoint::new(parent.compute_txid(), customer[0].0 as u32);
		let operator_script = srv.watchmand().config().sweep_address.clone().unwrap().require_network(Network::Regtest).unwrap().script_pubkey();
		for output in &parent.output {
			assert!(output.script_pubkey == registration || output.script_pubkey == operator_script || output.is_p2a_fee_anchor());
		}
		let mut parent_value = Amount::ZERO;
		let mut parent_inputs = std::collections::BTreeSet::new();
		for input in &parent.input {
			assert!(parent_inputs.insert(input.previous_output));
			let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
			parent_value += previous.output[input.previous_output.vout as usize].value;
		}
		assert_eq!(parent_value, parent.output.iter().map(|output| output.value).sum::<Amount>());
		let operator_value = parent.output.iter().filter(|output| output.script_pubkey == operator_script).map(|output| output.value).sum::<Amount>();
		let anchors = parent.output.iter().enumerate().filter(|(_, output)| output.is_p2a_fee_anchor()).collect::<Vec<_>>();
		assert_eq!(anchors.len(), 1);
		let anchor = OutPoint::new(parent.compute_txid(), anchors[0].0 as u32);
		let confirmed = ctx.generate_blocks(6).await;
		srv.wait_for_sync_height(confirmed).await;
		srv.watchmand().wait_for_sync_height(confirmed).await;
		srv.watchmand().trigger_sweep().await;
		let core = ctx.bitcoind().sync_client();
		let info = core.custom_get_raw_transaction_info(parent.compute_txid(), None).unwrap().unwrap();
		assert!(info.confirmations.unwrap_or(0) >= 6);
		let block_hash = info.blockhash.unwrap();
		let height = core.get_block_header_info(&block_hash).unwrap().height;
		assert_eq!(core.get_block_hash(height as u64).unwrap(), block_hash);
		let block = core.get_block(&block_hash).unwrap();
		assert!(block.txdata.contains(&parent));
		let children = block.txdata.iter().filter(|tx| tx.input.iter().any(|input| input.previous_output == anchor)).collect::<Vec<_>>();
		assert_eq!(children.len(), 1);
		let child = children[0];
		let mut child_value = Amount::ZERO;
		let mut child_inputs = std::collections::BTreeSet::new();
		for input in &child.input {
			assert!(child_inputs.insert(input.previous_output));
			assert!(!parent_inputs.contains(&input.previous_output));
			let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
			child_value += previous.output[input.previous_output.vout as usize].value;
		}
		let external = child.input.iter().filter(|input| input.previous_output != anchor).collect::<Vec<_>>();
		assert_eq!(external.len(), 1);
		assert!(sponsors.contains(&external[0].previous_output));
		let change = child.output.iter().map(|output| output.value).sum::<Amount>();
		let miner_fee = child_value.checked_sub(change).unwrap();
		assert!(miner_fee > Amount::ZERO);
		assert_eq!(source.amount(), sat(99_000) + sat(1_000));
		// This cut includes the customer parent and its one fee sponsor.
		// The separate cancelled-round tree and its sponsor are outside this equation.
		assert_eq!(parent_value + sat(100_000), sat(99_000) + operator_value + change + miner_fee);
		let paid = core.get_tx_out(&payout.txid, payout.vout, Some(true)).unwrap().unwrap();
		assert!(paid.confirmations >= 6);
		assert_eq!(paid.value, sat(99_000));
		assert_eq!(paid.script_pub_key.script().unwrap(), registration);
		let mut outputs = std::collections::BTreeMap::new();
		let mut transactions = std::collections::BTreeMap::new();
		for height in first_height + 1..=core.get_block_count().unwrap() {
			for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata { transactions.insert(tx.compute_txid(), tx); }
		}
		for txid in core.get_raw_mempool().unwrap() { transactions.insert(txid, ctx.bitcoind().await_transaction(txid).await); }
		for (txid, tx) in transactions {
			for (vout, output) in tx.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == registration) {
				outputs.insert(OutPoint::new(txid, vout as u32), output.value);
			}
		}
		assert_eq!(outputs, std::collections::BTreeMap::from([(payout, sat(99_000))]));
		let wallet = Box::pin(reopen_refresh(&ctx, &proxy, &mnemonic, database.clone())).await;
		wallet.chain().invalidate_caches().await;
		wallet.sync_fallback_payouts().await.unwrap();
		assert_eq!(wallet.get_vtxo_by_id(source.id()).await.unwrap().state, VtxoState::Spent);
		assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
		assert_refresh_balance(&wallet, Amount::ZERO, Amount::ZERO, Amount::ZERO, Amount::ZERO).await;
		assert_eq!(database.get_movement_by_id(movement).await.unwrap().status, MovementStatus::Failed);
		assert_eq!(database.get_movement_by_id(movement).await.unwrap().effective_balance, bitcoin::SignedAmount::ZERO);
		assert_eq!(database.get_movement_by_id(movement).await.unwrap().offchain_fee, Amount::ZERO);
		let current_history = wallet.history().await.unwrap();
		assert_eq!(current_history, history, "chain discovery preserves the original native action history");
		let refreshes = current_history.iter().filter(|entry| entry.subsystem.name == "bark.round").collect::<Vec<_>>();
		assert_eq!(refreshes.len(), 1);
		assert_eq!(refreshes[0].id, movement);
		let receipts = database.get_all_vtxos().await.unwrap();
		assert!(receipts.iter().any(|r| r.id() == source.id() && r.fallback_status == Some(bark::vtxo::FallbackStatus::Spent)));
		assert!(!receipts.iter().any(|r| r.id() == source.id() && r.fallback_status == Some(bark::vtxo::FallbackStatus::Pending)));
		assert_eq!(source.amount() - sat(1_000) - paid.value, Amount::ZERO,
			"the canonical 99,000-sat output and 1,000-sat charge leave no forfeited principal");
		wallet.sync_onchain().await.unwrap();
		let own_payouts = {
			let onchain = wallet.onchain().unwrap();
			let guard = onchain.read().await;
			let bdk = (&*guard as &dyn std::any::Any).downcast_ref::<OnchainWallet>().unwrap();
			bdk.list_unspent().into_iter().filter(|coin| coin.outpoint == payout)
				.map(|coin| coin.txout.clone()).collect::<Vec<_>>()
		};
		assert_eq!(own_payouts.len(), 1);
		assert_eq!(own_payouts[0].value, sat(99_000));
		assert_eq!(own_payouts[0].script_pubkey, registration);
		assert_eq!(transport.accepted_forfeits.load(Ordering::SeqCst), 0);
	})).await.expect("cancelled refresh and absent-owner payout must finish within the test budget");
}

#[tokio::test]
async fn issued_refresh_keeps_its_holder_without_exact_cancellation_metadata() {
	tokio::time::timeout(Duration::from_secs(180), Box::pin(async {
		let ctx = TestContext::new("bark_sdk/cancelled_refresh_metadata").await;
		let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().funded(btc(1))
			.cfg(|config| { config.expiry_payouts = true; config.vtxo_lifetime = BlockDelta::new(48); config.vtxo_exit_delta = BlockDelta::new(3); })
			.watchmand().create().await;
		let transport = RefreshTransport::default();
		let fixture = Box::pin(issued_refresh(&ctx, &srv, transport.clone(), RoundWriteFault::default(), false)).await;
		*transport.status_fault.lock().unwrap() = CancellationStatusFault::IssuedThenUnknown;
		transport.status_steps.store(0, Ordering::SeqCst);
		let mut state = fixture.wallet.lock_wait_round_state(fixture.state_id).await.unwrap().unwrap();
		assert!(state.sync(&fixture.wallet).await.is_err());
		assert_eq!(transport.status_steps.load(Ordering::SeqCst), 2);
		assert_eq!(state.state().flow_kind(), RoundFlowKind::DelegatedPending);
		drop(state);
		assert_refresh_held(&fixture.wallet, &fixture.database, fixture.state_id, fixture.movement, fixture.source.id(), RoundFlowKind::DelegatedPending).await;
		*transport.status_fault.lock().unwrap() = CancellationStatusFault::Pass;
		await_refresh_tombstone(&ctx, &srv, fixture.unlock_hash, fixture.sweep_at,
			RoundParticipationStatusAttestation::new(fixture.unlock_hash, &fixture.wallet.mailbox_keypair())).await;
		for fault in [CancellationStatusFault::Generic, CancellationStatusFault::MissingReason,
			CancellationStatusFault::MissingHash, CancellationStatusFault::WrongReason, CancellationStatusFault::MalformedHash,
			CancellationStatusFault::WrongHash, CancellationStatusFault::MultipleHashes, CancellationStatusFault::DuplicateHash,
			CancellationStatusFault::DuplicateReason, CancellationStatusFault::Unavailable] {
			*transport.status_fault.lock().unwrap() = fault;
			let before = transport.status_faults.load(Ordering::SeqCst);
			let mut state = fixture.wallet.lock_wait_round_state(fixture.state_id).await.unwrap().unwrap();
			assert!(state.sync(&fixture.wallet).await.is_err());
			assert_eq!(state.state().flow_kind(), RoundFlowKind::DelegatedPending);
			drop(state);
			assert!(transport.status_faults.load(Ordering::SeqCst) > before);
			assert_refresh_held(&fixture.wallet, &fixture.database, fixture.state_id, fixture.movement, fixture.source.id(), RoundFlowKind::DelegatedPending).await;
		}
		*transport.status_fault.lock().unwrap() = CancellationStatusFault::Pass;
		let statuses = fixture.wallet.sync_pending_rounds().await.unwrap();
		assert!(matches!(statuses.get(&fixture.state_id), Some(RoundStatus::Canceled)));
		assert_eq!(fixture.wallet.get_vtxo_by_id(fixture.source.id()).await.unwrap().state, VtxoState::Spendable);
		assert_refresh_balance(&fixture.wallet, Amount::ZERO, sat(100_000), Amount::ZERO, Amount::ZERO).await;
		assert_eq!(fixture.database.get_movement_by_id(fixture.movement).await.unwrap().status, MovementStatus::Failed);
		assert_eq!(fixture.database.get_movement_by_id(fixture.movement).await.unwrap().effective_balance, bitcoin::SignedAmount::ZERO);
		assert_eq!(fixture.database.get_movement_by_id(fixture.movement).await.unwrap().offchain_fee, Amount::ZERO);
		assert_eq!(transport.accepted_forfeits.load(Ordering::SeqCst), 0);
	})).await.expect("invalid cancellation replies must leave the principal protected");
}

#[tokio::test]
async fn fallback_source_state_write_failure_retains_its_receipt_fence() {
	Box::pin(retained_claim_write_failure(false)).await;
}

async fn reopen_refresh(
	ctx: &TestContext, proxy: &captaind::proxy::ArkRpcProxyServer,
	mnemonic: &bip39::Mnemonic, database: Arc<RefreshDatabase>,
) -> Wallet {
	let onchain = OnchainWallet::load_or_create(Network::Regtest, mnemonic.to_seed(""), database.clone()).await.unwrap();
	let mut config = ctx.bark_default_cfg(proxy, Some(ctx.bitcoind()));
	config.daemon_manual_sync = true;
	config.vtxo_exit_margin = BlockDelta::new(2);
	Wallet::open(Network::Regtest, WalletSeed::new_from_mnemonic(Network::Regtest, mnemonic), config,
		OpenWalletArgs { persister: Some(database), onchain: Some(Arc::new(tokio::sync::RwLock::new(onchain))), run_daemon: false, ..Default::default() }).await.unwrap()
}

async fn issued_refresh(
	ctx: &TestContext, srv: &Captaind, transport: RefreshTransport, fault: RoundWriteFault,
	interactive: bool,
) -> IssuedRefresh {
	transport.reject_nonces.store(true, Ordering::SeqCst);
	let proxy = srv.start_proxy_no_mailbox(transport.clone()).await;
	let storage_path = ctx.datadir.join("refresh-owner.json");
	let database = Arc::new(StorageAdaptorWrapper::new(RoundWriteGate {
		inner: FileStorageAdaptor::open(&storage_path).await.unwrap(), fault,
	}));
	let mnemonic = bip39::Mnemonic::generate(12).unwrap();
	let wallet = Box::pin(reopen_refresh(ctx, &proxy, &mnemonic, database.clone())).await;
	let address = wallet.onchain().unwrap().write().await.address().await.unwrap();
	ctx.bitcoind().fund_addr(address, sat(300_000)).await;
	ctx.generate_blocks(1).await;
	wallet.sync_onchain().await.unwrap();
	let board = wallet.board_amount(sat(100_000)).await.unwrap();
	assert_eq!(board.vtxos.len(), 1);
	ctx.await_transaction(board.funding_tx.compute_txid()).await;
	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	wallet.sync_pending_boards().await.unwrap();
	let source = wallet.get_full_vtxo(board.vtxos[0]).await.unwrap();
	let funding = ctx.bitcoind().await_transaction(source.chain_anchor().txid).await;
	source.validate(&funding).unwrap();
	assert_eq!(source.amount(), sat(100_000));
	let first_height = ctx.bitcoind().get_block_count().await;
	let round = if interactive {
		let participation = wallet.build_refresh_participation([source.id()]).await.unwrap().unwrap();
		wallet.join_next_round(participation, Some(RoundMovement::Refresh)).await.unwrap().unlock()
	} else {
		wallet.refresh_vtxos_delegated(vec![source.id()]).await.unwrap().unwrap()
	};
	let state_id = round.id();
	drop(round);
	let movement = wallet.history().await.unwrap().into_iter().find(|movement|
		movement.subsystem.name == "bark.round" && movement.input_vtxos == [source.id()]).unwrap().id;
	let mut finished = srv.subscribe_log::<RoundFinished>();
	let mut events = wallet.subscribe_round_events().await.unwrap();
	srv.trigger_round().await;
	if interactive {
		loop {
			let event = events.next().wait_millis(30_000).await.unwrap().unwrap();
			wallet.progress_pending_rounds(Some(&event)).await.unwrap();
			if matches!(event, RoundEvent::Finished(_)) { break; }
		}
		let saved = database.get_round_state_by_id(state_id).await.unwrap().unwrap();
		assert_eq!(saved.state().flow_kind(), RoundFlowKind::AwaitingConfirmations);
		let outputs = saved.state().output_vtxos().unwrap();
		assert!(!outputs.is_empty(), "the real finished event must persist replacement bodies");
		assert_eq!(outputs.iter().map(|output| output.amount()).sum::<Amount>(), sat(100_000));
		assert!(!saved.state().forfeits_may_be_exposed());
	}
	drop(events);
	let finished = finished.recv().wait(Duration::from_secs(30)).await.unwrap();
	ctx.await_transaction(finished.txid).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	wallet.chain().invalidate_caches().await;
	let saved = database.get_round_state_by_id(state_id).await.unwrap().unwrap();
	let unlock_hash = saved.state().unlock_hash().unwrap();
	drop(saved);
	let response = srv.get_public_rpc().await.round_participation_status(protos::RoundParticipationStatusRequest {
		unlock_hash: unlock_hash.to_byte_array().to_vec(),
		attestation: RoundParticipationStatusAttestation::new(unlock_hash, &wallet.mailbox_keypair()).serialize(),
	}).await.unwrap().into_inner();
	assert_eq!(response.status, protos::RoundParticipationStatus::RoundPartIssued as i32);
	assert!(response.unlock_preimage.is_none());
	let replacement_expiry = response.output_vtxos.iter().map(|bytes| Vtxo::<Full>::deserialize(bytes).unwrap().expiry_height()).max().unwrap();
	let sweep_at = source.expiry_height().max(replacement_expiry) + BlockDelta::new(1);
	let mut locked = wallet.lock_wait_round_state(state_id).await.unwrap().unwrap();
	let error = locked.sync(&wallet).await.err().expect("the nonce transport fault must interrupt the real issued refresh");
	assert!(format!("{error:#}").contains("nonce transport unavailable"));
	assert!(!locked.state().forfeits_may_be_exposed());
	assert_eq!(locked.state().flow_kind(), if interactive {
		RoundFlowKind::AwaitingConfirmations
	} else { RoundFlowKind::DelegatedPending });
	drop(locked);
	assert!(transport.nonce_requests.load(Ordering::SeqCst) > 0);
	assert_eq!(transport.accepted_forfeits.load(Ordering::SeqCst), 0);
	let fixture = IssuedRefresh { wallet, proxy, database, storage_path, mnemonic, source, state_id, movement, unlock_hash, sweep_at, first_height };
	assert_refresh_held(&fixture.wallet, &fixture.database, fixture.state_id, fixture.movement, fixture.source.id(),
		if interactive { RoundFlowKind::AwaitingConfirmations } else { RoundFlowKind::DelegatedPending }).await;
	fixture
}

async fn assert_refresh_held(
	wallet: &Wallet, database: &RefreshDatabase, id: RoundStateId, movement: MovementId,
	source: VtxoId, expected_flow: RoundFlowKind,
) {
	let saved = database.get_round_state_by_id(id).await.unwrap().unwrap();
	assert_eq!(saved.state().flow_kind(), expected_flow);
	if saved.state().flow_kind() == RoundFlowKind::AwaitingConfirmations {
		assert!(!saved.state().output_vtxos().unwrap().is_empty());
	}
	assert!(!saved.state().forfeits_may_be_exposed());
	assert_eq!(wallet.get_vtxo_by_id(source).await.unwrap().state, VtxoState::Locked { holder: Some(movement.into()) });
	assert_eq!(database.get_movement_by_id(movement).await.unwrap().status, MovementStatus::Pending);
	assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
	assert_refresh_balance(wallet, Amount::ZERO, Amount::ZERO, sat(100_000), Amount::ZERO).await;
}

async fn assert_refresh_balance(
	wallet: &Wallet, spendable: Amount, needs_refresh: Amount,
	pending_in_round: Amount, fallback_principal: Amount,
) {
	wallet.chain().invalidate_caches().await;
	let balance = wallet.balance().await.unwrap();
	let expected = [spendable, needs_refresh, pending_in_round,
		Amount::ZERO, Amount::ZERO, Amount::ZERO, Amount::ZERO, Amount::ZERO, Amount::ZERO];
	assert_eq!([
		balance.spendable, balance.needs_refresh, balance.pending_in_round,
		balance.pending_lightning_send, balance.claimable_lightning_receive, balance.pending_exit,
		balance.pending_board, balance.pending_arkoor_send, balance.pending_offboard,
	], expected, "the remaining Ark principal must occupy exactly its expected balance bucket");
	// This fixture has one original source, no source/change aliases. Its
	// known Pending body retains principal without inventing another balance.
	assert_eq!(wallet.all_vtxos().await.unwrap().iter()
		.filter(|coin| coin.fallback_status == Some(bark::vtxo::FallbackStatus::Pending))
		.map(|coin| coin.amount()).sum::<Amount>(), fallback_principal);
	let total = expected.into_iter().sum::<Amount>();
	assert_eq!(balance.total(), total);
	assert_eq!(balance.pending(), pending_in_round);
	let summary = balance.summary();
	assert_eq!(summary.spendable, spendable);
	assert_eq!(summary.needs_refresh, needs_refresh);
	assert_eq!(summary.pending, pending_in_round);
	assert_eq!(summary.total, total);
}

async fn await_refresh_tombstone(
	ctx: &TestContext, srv: &Captaind, unlock_hash: UnlockHash, sweep_at: BlockHeight,
	attestation: RoundParticipationStatusAttestation,
) {
	let tip = ctx.bitcoind().get_block_count().await as u32;
	ctx.generate_blocks(sweep_at.to_u32().saturating_sub(tip)).await;
	srv.wait_for_sync_height(sweep_at).await;
	srv.watchmand().wait_for_sync_height(sweep_at).await;
	async {
		loop {
			srv.watchmand().trigger_sweep().await;
			let response = srv.get_public_rpc().await.round_participation_status(protos::RoundParticipationStatusRequest {
				unlock_hash: unlock_hash.to_byte_array().to_vec(),
				attestation: attestation.serialize(),
			}).await;
			if let Err(error) = response {
				assert_eq!(error.code(), tonic::Code::NotFound);
				assert_eq!(error.metadata().get("abandon-reason").unwrap().to_str().unwrap(), "round_cancelled_before_forfeit");
				assert_eq!(error.metadata().get("identifiers").unwrap().to_str().unwrap(), unlock_hash.to_string());
				assert_eq!(error.metadata().get_all("identifiers").iter().count(), 1);
				assert_eq!(error.metadata().get_all("abandon-reason").iter().count(), 1);
				break;
			}
			tokio::time::sleep(poll_interval()).await;
		}
	}.wait_millis(30_000).await;
}

async fn retained_claim_write_failure(receipt_write: bool) {
	tokio::time::timeout(Duration::from_secs(180), Box::pin(async {
		let name = if receipt_write { "bark_sdk/receipt_write_failure" }
			else { "bark_sdk/receipt_source_state_write_failure" };
		let ctx = TestContext::new(name).await;
		let srv = ctx.captaind("server").no_vtxo_pool().cfg(|config| { config.expiry_payouts = true;
			config.vtxo_lifetime = BlockDelta::new(24);
			config.vtxo_exit_delta = BlockDelta::new(3);
		}).watchmand().create().await;
		let transport = RefreshTransport::default();
		let proxy = srv.start_proxy_no_mailbox(transport.clone()).await;
		let seed = bip39::Mnemonic::generate(12).unwrap();
		let fault = RoundWriteFault::default();
		let path = ctx.datadir.join("retained-owner.json");
		let database = Arc::new(StorageAdaptorWrapper::new(RoundWriteGate {
			inner: FileStorageAdaptor::open(&path).await.unwrap(), fault: fault.clone(),
		}));
		let wallet = Box::pin(reopen_refresh(&ctx, &proxy, &seed, database.clone())).await;
		let address = wallet.onchain().unwrap().write().await.address().await.unwrap();
		ctx.bitcoind().fund_addr(address, sat(300_000)).await;
		ctx.generate_blocks(1).await;
		wallet.sync_onchain().await.unwrap();
		let board = wallet.board_amount(sat(100_000)).await.unwrap();
		ctx.await_transaction(board.funding_tx.compute_txid()).await;
		ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
		wallet.sync_pending_boards().await.unwrap();
		let coin = wallet.get_vtxo_by_id(board.vtxos[0]).await.unwrap();
		let signed = wallet.get_full_vtxo(coin.id()).await.unwrap();
		wallet.validate_vtxo(&signed).await.unwrap();
		super::fallback_adoption::await_decision(&ctx, &srv, &coin).await;
		if receipt_write { fault.snapshot_armed.store(true, Ordering::SeqCst); }
		else { fault.unlock_armed.store(true, Ordering::SeqCst); }
		assert!(wallet.sync_fallback_payouts().await.is_err());
		assert_eq!(if receipt_write { fault.snapshot_hits.load(Ordering::SeqCst) }
			else { fault.unlock_hits.load(Ordering::SeqCst) }, 1);
		assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), signed.serialize());
		assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().state, VtxoState::Spendable);
		assert!(database.get_all_vtxos().await.unwrap().iter().all(|coin| coin.fallback_status.is_none()),
			"both former receipt/source fault selectors now fail the single atomic known-coin write");
		// No payment/exit is attempted after the failed persistence operation.
		// Reopen the same store and retry only authenticated observation.
		drop(wallet);
		drop(database);
		fault.snapshot_armed.store(false, Ordering::SeqCst);
		fault.unlock_armed.store(false, Ordering::SeqCst);
		let database = Arc::new(StorageAdaptorWrapper::new(RoundWriteGate {
			inner: FileStorageAdaptor::open(&path).await.unwrap(), fault,
		}));
		let wallet = Box::pin(reopen_refresh(&ctx, &proxy, &seed, database.clone())).await;
		wallet.sync_fallback_payouts().await.unwrap();
		assert_eq!(wallet.get_vtxo_by_id(coin.id()).await.unwrap().state, VtxoState::Locked { holder: None });
		assert_eq!(wallet.get_full_vtxo(coin.id()).await.unwrap().serialize(), signed.serialize());
		wallet.validate_vtxo(&signed).await.unwrap();
		assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
		assert_refresh_balance(&wallet, Amount::ZERO, Amount::ZERO, Amount::ZERO, sat(100_000)).await;
		let committed = database.get_all_vtxos().await.unwrap().into_iter()
			.filter(|coin| coin.fallback_status.is_some()).collect::<Vec<_>>();
		assert_eq!(committed.len(), 1);
		assert_eq!(committed[0].id(), coin.id());
		assert_eq!(committed[0].fallback_status, Some(bark::vtxo::FallbackStatus::Pending));
		assert!(!wallet.history().await.unwrap().iter().any(|movement| movement.subsystem.name == "scuttle.fallback"),
			"a status write does not add an Ark receipt movement");
	})).await.expect("receipt persistence failures retain the source and retry only observation");
}
