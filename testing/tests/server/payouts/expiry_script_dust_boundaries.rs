//! Exact selected P2TR dust boundaries through ordinary SDK accounts.

use super::expiry_test::{ExpectedLeaf, confirm_epoch, receipt, rebuilt, actual_fee, Epoch};

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use ark::attestations::VtxoStatusAttestation;
use ark::{ProtocolEncoding, Vtxo, VtxoId, SECP};
use ark::vtxo::Full;
use ark_testing::{require_bitcoind_chain_source, sat, Captaind, TestContext};
use bark::Wallet;
use bark::movement::{Movement, MovementStatus};
use bark::vtxo::VtxoState;
use bitcoin::{Amount, OutPoint, ScriptBuf, Transaction};
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::RpcApi;
use server::database::Db;
use server_rpc::protos;

async fn balances(wallet: &Wallet, spendable: Amount) {
	let balance = ark_testing::balance::assert_balance_consistent(wallet, false).await;
	assert_eq!(balance.spendable, spendable);
	assert_eq!(balance.total(), spendable);
	assert!(wallet.exit_mgr().list_all(false, false).await.unwrap().is_empty(),
		"no live or finished unilateral exit may claim already settled principal");
}

/// Normal SDK sends, mailbox hydration, and authenticated ordinary status
/// establish the source and its complete signed descendants. No manually fabricated registrations.
async fn receive_boundary(
	srv: &Captaind, sender: &Wallet, recipient: &Wallet,
	amount: Amount, board: &Transaction,
) -> (Vtxo<Full>, Vec<VtxoId>) {
	balances(recipient, Amount::ZERO).await;
	let before = sender.spendable_vtxos().await.unwrap();
	let total_before = before.iter().map(|coin| coin.amount()).sum::<Amount>();
	let address = recipient.new_address().await.unwrap();
	Box::pin(sender.send_arkoor_payment(&address, amount)).await.unwrap();
	assert!(sender.pending_arkoor_sends().await.unwrap().is_empty());
	recipient.sync_mailbox().await.unwrap();
	let received = recipient.spendable_vtxos().await.unwrap();
	assert_eq!(received.len(), 1, "a successful mailbox call alone is insufficient");
	let leaf = recipient.get_full_vtxo(received[0].id()).await.unwrap();
	assert_eq!(leaf.amount(), amount);
	assert_eq!(leaf.policy(), address.policy());
	leaf.validate(board).unwrap();
	let _real_leaf_key = recipient.get_vtxo_key(&leaf).await.unwrap();
	balances(recipient, amount).await;
	let after = sender.spendable_vtxos().await.unwrap();
	assert_eq!(after.iter().map(|coin| coin.amount()).sum::<Amount>() + amount, total_before);
	let consumed = before.iter().filter(|coin| !after.iter().any(|current| current.id() == coin.id())).collect::<Vec<_>>();
	assert_eq!(consumed.len(), 1, "all genuine change pieces here exceed the next small payment");
	let source = sender.get_full_vtxo(consumed[0].id()).await.unwrap();
	assert_eq!(sender.get_vtxo_by_id(source.id()).await.unwrap().state, VtxoState::Spent);
	assert_eq!(leaf.chain_anchor(), source.chain_anchor());
	assert_eq!(leaf.expiry_height(), source.expiry_height());
	assert_eq!(leaf.exit_delta(), source.exit_delta());
	assert_eq!(leaf.server_pubkey(), source.server_pubkey());
	let new_change = after.iter().filter(|coin| !before.iter().any(|prior| prior.id() == coin.id())).collect::<Vec<_>>();
	assert_eq!(new_change.len(), 2, "retain the real default split factor2");
	assert_eq!(leaf.amount() + new_change.iter().map(|coin| coin.amount()).sum::<Amount>(), source.amount());
	let key = sender.get_vtxo_key(&source).await.unwrap();
	let status = srv.get_public_rpc().await.get_vtxo_status(protos::GetVtxoStatusRequest {
		vtxo_id: source.id().to_bytes().to_vec(),
		attestation: VtxoStatusAttestation::new(source.id(), &key).serialize(),
	}).await.unwrap().into_inner();
	assert_eq!(status.spend_state, protos::VtxoSpendState::Spent as i32);
	let arrivals = recipient.history().await.unwrap();
	let arrivals = arrivals.iter().filter(|movement| movement.subsystem.name == "bark.arkoor"
		&& movement.subsystem.kind == "receive" && movement.output_vtxos.contains(&leaf.id())).collect::<Vec<_>>();
	assert_eq!(arrivals.len(), 1);
	assert_eq!(arrivals[0].status, MovementStatus::Successful);
	assert_eq!(arrivals[0].effective_balance.to_sat(), amount.to_sat() as i64);
	(leaf, vec![source.id()])
}

async fn verify_return(wallet: &Wallet, leaves: &[&ExpectedLeaf], epoch: &Epoch, baseline: &[Movement]) {
	wallet.sync_fallback_payouts().await.unwrap();
	for leaf in leaves { receipt(wallet, leaf, epoch).await; }
	balances(wallet, Amount::ZERO).await;
	assert!(wallet.pending_arkoor_sends().await.unwrap().is_empty());
	assert_eq!(wallet.history().await.unwrap(), baseline,
		"chain payout discovery must retain ordinary history without duplicate debits");
}

async fn boundaries() {
	let ctx = TestContext::new("server/expiry_script_dust_boundaries_p2tr").await;
	let start = ctx.bitcoind().get_block_count().await;
	let srv = Box::pin(ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().cfg(|config| { config.expiry_payouts = true;
		config.vtxo_lifetime = BlockDelta::new(40);
		config.vtxo_exit_delta = BlockDelta::new(2);
	}).watchmand().create()).await;
	let donor_cli = Box::pin(ctx.bark("original_sender", &srv)
		.cfg(|config| config.daemon_manual_sync = true).funded(sat(200_000)).create()).await;
	let ids = Box::pin(donor_cli.board_and_confirm_and_register(&ctx, sat(100_000))).await;
	assert_eq!(ids.len(), 1);
	let sender = Box::pin(donor_cli.client()).await;
	assert_eq!(sender.config().change_vtxo_split_factor, 2);
	sender.stop_daemon_wait().await.unwrap();
	let source = sender.get_full_vtxo(ids[0]).await.unwrap();
	assert_eq!(source.amount(), sat(100_000));
	let root = source.chain_anchor();
	let board = ctx.bitcoind().await_transaction(root.txid).await;
	source.validate(&board).unwrap();
	let ordinary = donor_cli.onchain_utxos().await.into_iter().map(|coin| (coin.outpoint, coin.amount)).collect::<BTreeMap<_, _>>();
	assert!(!ordinary.is_empty() && ordinary.keys().all(|point| point.txid == root.txid && *point != root));
	let board_fee = actual_fee(&ctx, &board).await;
	assert_eq!(sat(100_000) + ordinary.values().copied().sum::<Amount>() + board_fee, sat(200_000));
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let mut apps = Vec::new();
	let mut recipients = Vec::new();
	for index in 0..3 {
		let app = Box::pin(ctx.bark(format!("recipient_{index}"), &srv)
			.cfg(|config| config.daemon_manual_sync = true).create()).await;
		recipients.push(Box::pin(app.client()).await);
		apps.push(app);
	}
	let sender_binding = sender.fallback_script();
	let mut bindings = vec![sender_binding.clone()];
	let mut mailboxes = vec![sender.mailbox_identifier()];
	for wallet in &recipients {
		assert!(wallet.onchain().is_none());
		bindings.push(wallet.fallback_script());
		mailboxes.push(wallet.mailbox_identifier());
	}
	assert_eq!(bindings.iter().map(|binding| binding.clone()).collect::<BTreeSet<_>>().len(), 4);
	assert_eq!(mailboxes.iter().map(|mailbox| mailbox.as_pubkey().to_string()).collect::<BTreeSet<_>>().len(), 4);
	for (binding, mailbox) in bindings.iter().zip(&mailboxes) {
		assert_eq!(binding.minimal_non_dust(), sat(330));
		assert_eq!(**binding, ScriptBuf::new_p2tr(&SECP, mailbox.as_pubkey().x_only_public_key().0, None));
		for input in &board.input {
			let funding = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
			assert!(funding.output.iter().all(|output| &output.script_pubkey != binding));
		}
	}
	let dust = 330;
	let amounts = [1_999 + dust, 2_000 + dust, 2_001 + dust];
	let mut leaves = Vec::new();
	let mut old = BTreeSet::from([source.id()]);
	for (index, wallet) in recipients.iter().enumerate() {
		let (coin, consumed) = Box::pin(receive_boundary(&srv, &sender, wallet, sat(amounts[index]), &board)).await;
		old.extend(consumed);
		leaves.push(ExpectedLeaf { coin, mailbox: mailboxes[index + 1], script: bindings[index + 1].clone(),
			fee: 2_000, payout: dust + (index as u64).saturating_sub(1), subsidy: if index == 0 { 1 } else { 0 },
			forfeited: 0, group_payout: dust + (index as u64).saturating_sub(1) });
	}
	let mut change = Vec::new();
	for coin in sender.spendable_vtxos().await.unwrap() { change.push(sender.get_full_vtxo(coin.id()).await.unwrap()); }
	change.sort_by_key(|coin| coin.id());
	assert_eq!(change.len(), 4);
	let change_gross = change.iter().map(|coin| coin.amount().to_sat()).sum::<u64>();
	assert_eq!(change_gross + amounts.iter().sum::<u64>(), 100_000);
	let mut fee = 2_000;
	for coin in change {
		let charged = coin.amount().to_sat().min(fee);
		fee -= charged;
		let payout = coin.amount().to_sat() - charged;
		leaves.push(ExpectedLeaf { coin, mailbox: mailboxes[0], script: sender_binding.clone(), fee: charged,
			payout, subsidy: 0, forfeited: 0, group_payout: change_gross - 2_000 });
	}
	assert_eq!(fee, 0);
	assert_eq!(leaves.iter().map(|leaf| leaf.coin.amount()).sum::<Amount>(), sat(100_000));
	assert_eq!(leaves.iter().map(|leaf| leaf.fee).sum::<u64>(), 8_000);
	assert_eq!(leaves.iter().map(|leaf| leaf.subsidy).sum::<u64>(), 1);
	assert_eq!(leaves.iter().map(|leaf| leaf.forfeited).sum::<u64>(), 0);
	assert_eq!(leaves.iter().map(|leaf| leaf.payout).sum::<u64>(), 92_001);
	balances(&sender, sat(change_gross)).await;
	for leaf in &leaves {
		assert_eq!(leaf.coin.chain_anchor(), root);
		leaf.coin.validate(&board).unwrap();
		assert!(leaf.coin.transactions().len() <= 8);
		old.extend(leaf.coin.ancestor_ids());
	}
	let current = leaves.iter().map(|leaf| leaf.coin.id()).collect::<BTreeSet<_>>();
	old.retain(|id| !current.contains(id));
	let baselines = std::iter::once(&sender).chain(&recipients).map(|wallet| wallet.history());
	let mut history_before = Vec::new();
	for pending in baselines { history_before.push(pending.await.unwrap()); }
	assert!(history_before.iter().all(|history| history.iter().all(|movement| movement.subsystem.name != "abandon.fallback_payout")));
	for wallet in std::iter::once(&sender).chain(&recipients) { wallet.stop_daemon_wait().await.unwrap(); }
	drop(sender);
	drop(recipients);
	let core = ctx.bitcoind().sync_client();
	let mut sponsors = BTreeMap::new();
	for _ in 0..2 {
		let address = srv.watchmand().wait_wallet_address().await;
		let txid = ctx.bitcoind().fund_addr(address.clone(), sat(100_000)).await;
		let transaction = ctx.bitcoind().await_transaction(txid).await;
		let points = transaction.output.iter().enumerate().filter(|(_, output)| output.value == sat(100_000)
			&& output.script_pubkey == address.script_pubkey()).collect::<Vec<_>>();
		assert_eq!(points.len(), 1);
		assert!(sponsors.insert(OutPoint::new(txid, points[0].0 as u32), points[0].1.clone()).is_none());
	}
	ctx.generate_blocks(6).await;
	for (point, expected) in &sponsors {
		let actual = core.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
		assert!(actual.confirmations >= 6);
		assert_eq!(actual.value, expected.value);
		assert_eq!(actual.script_pub_key.script().unwrap(), expected.script_pubkey);
	}
	let backing = core.get_tx_out(&root.txid, root.vout, Some(true)).unwrap().unwrap();
	assert!(backing.confirmations >= 6);
	assert_eq!(backing.value, sat(100_000));
	let epoch = Box::pin(confirm_epoch(&ctx, &srv, &db, &leaves, start, 90, 96)).await;
	assert_eq!(epoch.parents.len(), 1);
	assert!(epoch.children.is_empty());
	let parent = epoch.parents.values().next().unwrap();
	let payees = vec![(bindings[1].clone(), Some(dust)), (bindings[2].clone(), Some(dust)),
		(bindings[3].clone(), Some(dust + 1)), (sender_binding.clone(), Some(change_gross - 2_000))];
	let operator = {
		let config = srv.watchmand().config();
		config.sweep_address.clone().unwrap().require_network(config.network).unwrap().script_pubkey()
	};
	assert!(payees.iter().all(|(script, _)| script != &operator));
	let remainder = parent.output.iter().filter(|output| payees.iter().all(|(script, _)| output.script_pubkey != *script)).collect::<Vec<_>>();
	let miner_fee = actual_fee(&ctx, parent).await;
	assert_eq!(parent.output.iter().map(|output| output.value).sum::<Amount>() + miner_fee, sat(100_000));
	assert_eq!(remainder.iter().map(|output| output.value).sum::<Amount>() + miner_fee, sat(7_999));
	assert!(remainder.iter().all(|output| output.script_pubkey == operator));
	let forbidden = old.iter().map(ToString::to_string).collect::<Vec<_>>();
	assert!(db.read(async |tx| Ok(tx.query("SELECT c.vtxo_id FROM expiry_allocation_coin c \
		WHERE c.vtxo_id=ANY($1) AND c.disposition='customer'",
		&[&forbidden]).await?)).await.unwrap().is_empty());
	let returned_sender = Box::pin(donor_cli.client()).await;
	let sender_leaves = leaves.iter().filter(|leaf| leaf.mailbox == mailboxes[0]).collect::<Vec<_>>();
	verify_return(&returned_sender, &sender_leaves, &epoch, &history_before[0]).await;
	let mut returned = Vec::new();
	for (index, app) in apps.iter().enumerate() {
		assert!(app.datadir().join("db.sqlite").is_file(), "return must use the original wallet database");
		let wallet = Box::pin(app.client()).await;
		assert_eq!(wallet.fallback_script(), bindings[index + 1]);
		assert_eq!(wallet.mailbox_identifier(), mailboxes[index + 1]);
		verify_return(&wallet, &[&leaves[index]], &epoch, &history_before[index + 1]).await;
		returned.push(wallet);
	}
	for _ in 0..2 {
		verify_return(&returned_sender, &sender_leaves, &epoch, &history_before[0]).await;
		for (index, wallet) in returned.iter().enumerate() {
			verify_return(wallet, &[&leaves[index]], &epoch, &history_before[index + 1]).await;
		}
	}
	for wallet in std::iter::once(&returned_sender).chain(&returned) { wallet.stop_daemon_wait().await.unwrap(); }
	drop(returned_sender);
	drop(returned);
	ctx.generate_blocks(1).await;
	rebuilt(&ctx, &srv, &db).await;

	assert!(ctx.bitcoind().get_block_count().await - start <= 96);
}

#[tokio::test]
async fn p2tr_positive_subdust_fee_is_subsidized_to_pay_standard_output() {
	require_bitcoind_chain_source!();
	assert!(std::env::var_os("BARK_DOUBLE_DRIVE_ACTIONS").is_none());
	tokio::time::timeout(Duration::from_secs(180), Box::pin(boundaries())).await
		.expect("three genuine boundaries must fit180seconds without retries");
}

/// The 2,000-sat floor consumes a whole 1,500-sat coin: the payout has no
/// output for it, and the server and Bark both report a zero-payout forfeiture.
#[tokio::test]
async fn p2tr_fee_consuming_the_whole_coin_is_forfeited() {
	require_bitcoind_chain_source!();
	let ctx = TestContext::new("server/expiry_zero_output_forfeited").await;
	let start = ctx.bitcoind().get_block_count().await;
	let srv = Box::pin(ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().cfg(|config| { config.expiry_payouts = true;
		config.vtxo_lifetime = BlockDelta::new(40);
		config.vtxo_exit_delta = BlockDelta::new(2);
	}).watchmand().create()).await;
	let donor_cli = Box::pin(ctx.bark("sender", &srv)
		.cfg(|config| config.daemon_manual_sync = true).funded(sat(200_000)).create()).await;
	let ids = Box::pin(donor_cli.board_and_confirm_and_register(&ctx, sat(100_000))).await;
	let sender = Box::pin(donor_cli.client()).await;
	sender.stop_daemon_wait().await.unwrap();
	let board = ctx.bitcoind().await_transaction(sender.get_full_vtxo(ids[0]).await.unwrap().chain_anchor().txid).await;
	let app = Box::pin(ctx.bark("recipient", &srv).cfg(|config| config.daemon_manual_sync = true).create()).await;
	let recipient = Box::pin(app.client()).await;
	let (coin, _) = Box::pin(receive_boundary(&srv, &sender, &recipient, sat(1_500), &board)).await;
	let leaf = ExpectedLeaf { coin, mailbox: recipient.mailbox_identifier(), script: recipient.fallback_script(),
		fee: 1_500, payout: 0, subsidy: 0, forfeited: 0, group_payout: 0 };
	let history = recipient.history().await.unwrap();
	for wallet in [&sender, &recipient] { wallet.stop_daemon_wait().await.unwrap(); }
	drop(sender);
	drop(recipient);
	ctx.bitcoind().fund_addr(srv.watchmand().wait_wallet_address().await, sat(100_000)).await;
	ctx.generate_blocks(6).await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let epoch = Box::pin(confirm_epoch(&ctx, &srv, &db, std::slice::from_ref(&leaf), start, 90, 96)).await;
	let recipient = Box::pin(app.client()).await;
	let key = recipient.get_vtxo_key(&leaf.coin).await.unwrap();
	let status = srv.get_public_rpc().await.get_vtxo_status(protos::GetVtxoStatusRequest {
		vtxo_id: leaf.coin.id().to_bytes().to_vec(),
		attestation: VtxoStatusAttestation::new(leaf.coin.id(), &key).serialize(),
	}).await.unwrap().into_inner();
	assert_eq!(status.spend_state, protos::VtxoSpendState::Spent as i32);
	assert_eq!(status.fallback_payout, Some(protos::FallbackPayout {
		state: protos::FallbackState::Forfeited as i32, gross_sat: 1_500, fee_sat: 1_500, payout_sat: 0,
	}));
	verify_return(&recipient, &[&leaf], &epoch, &history).await;
}
