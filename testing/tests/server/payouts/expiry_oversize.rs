//! Real-size direct expiry payouts. Run the large case explicitly.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use ark::arkoor::{ArkoorDestination, package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse}};
use ark::mailbox::MailboxIdentifier;
use ark::vtxo::Full;
use ark::{ProtocolEncoding, ServerVtxoPolicy, Vtxo, SECP};
use ark_testing::{sat, Captaind, TestContext};
use ark_testing::constants::BOARD_CONFIRMATIONS;
use ark_testing::daemon::captaind::ArkClient;
use bitcoin::{Network, OutPoint, ScriptBuf, Transaction, TxOut};
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::{Keypair, rand::{thread_rng, RngCore}};
use bitcoin_ext::{BlockDelta, TxOutExt};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::Db;
use server_rpc::protos;

use super::expiry_test::actual_fee;

const PAYEES: usize = 327;
const LEAF_SAT: u64 = 5_000;

struct Owner {
	key: Keypair,
	mailbox: MailboxIdentifier,
	script: ScriptBuf,
}

struct Node {
	input: OutPoint,
	checkpoint_tx: Transaction,
	branches: Vec<Branch>,
	payees: usize,
}

struct Branch {
	checkpoint: OutPoint,
	exit_tx: Transaction,
	output: OutPoint,
}

fn owner() -> Owner {
	let mut seed = [0; 64];
	thread_rng().fill_bytes(&mut seed);
	let account = Xpriv::new_master(Network::Regtest, &seed).unwrap();
	let key = account.derive_priv(&SECP, &[ChildNumber::from_normal_idx(1).unwrap()]).unwrap().to_keypair(&SECP);
	let mailbox = account.derive_priv(&SECP, &[ChildNumber::from_hardened_idx(1).unwrap()]).unwrap().to_keypair(&SECP);
	let script = ScriptBuf::new_p2tr(&SECP, mailbox.x_only_public_key().0, None);
	Owner { key, mailbox: MailboxIdentifier::from_pubkey(mailbox.public_key()), script }
}

async fn native_node(
	srv: &Captaind, client: &bark::Wallet, rpc: &mut ArkClient, input: Vtxo<Full>, key: Keypair,
	destinations: Vec<ArkoorDestination>, payees: usize, owners: &[Owner],
) -> (Vec<Vtxo<Full>>, Node) {
	assert!(input.exit_depth() < 100);
	assert!(!destinations.is_empty() && destinations.len() <= 4);
	let input_point = input.point();
	let input_expiry = input.expiry_height();
	let input_exit_delta = input.exit_delta();
	let input_server = input.server_pubkey();
	let expected = destinations.iter().map(|d| (d.policy.clone(), d.total_amount)).collect::<Vec<_>>();
	let builder = ArkoorPackageBuilder::new_with_checkpoints([input], destinations).unwrap()
		.generate_user_nonces(&[key]).unwrap();
	let mut request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		let key = ark::VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey();
		if let Some(owner) = owners.iter().find(|owner| owner.key.public_key() == key) {
			output.mailbox_id = Some(owner.mailbox.serialize());
		}
	}
	let response = rpc.request_arkoor_cosign(super::signed_package(srv, &[client], request).await)
		.await.unwrap().into_inner();
	let signed = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap()).unwrap();
	let checkpoints = signed.build_signed_internal_vtxos().into_iter()
		.filter(|(v, _)| matches!(v.policy(), ServerVtxoPolicy::Checkpoint(_))).collect::<Vec<_>>();
	// The native checkpoint transaction fans out into one independently
	// recoverable checkpoint per destination. Each has its own signed exit
	// transaction; there is no single checkpoint containing all destinations.
	assert_eq!(checkpoints.len(), expected.len());
	let transactions = signed.signed_virtual_transactions();
	assert_eq!(transactions.len(), expected.len() + 1);
	let checkpoint_tx = transactions.iter().find(|tx| tx.input[0].previous_output == input_point).unwrap().clone();
	assert_eq!(checkpoint_tx.input.len(), 1);
	assert_eq!(checkpoint_tx.output.len(), expected.len() + 1);
	assert_eq!(checkpoint_tx.output.iter().filter(|out| out.is_p2a_fee_anchor()).count(), 1);
	let outputs = signed.build_signed_vtxos();
	assert_eq!(outputs.len(), expected.len());
	let mut branches = Vec::new();
	for (index, (vtxo, (policy, amount))) in outputs.iter().zip(expected).enumerate() {
		assert_eq!(vtxo.policy(), &policy);
		assert_eq!(vtxo.amount(), amount);
		assert!(vtxo.exit_depth() < 100);
		let point = OutPoint::new(checkpoint_tx.compute_txid(), index as u32);
		let (checkpoint, exit_txid) = checkpoints.iter().find(|(v, _)| v.point() == point).unwrap();
		assert_eq!(checkpoint.policy(), &ServerVtxoPolicy::new_checkpoint(key.public_key()));
		assert_eq!(checkpoint.amount(), amount);
		assert_eq!(checkpoint.expiry_height(), input_expiry);
		assert_eq!(checkpoint.exit_delta(), input_exit_delta);
		assert_eq!(checkpoint.server_pubkey(), input_server);
		assert_eq!(checkpoint.txout(), checkpoint_tx.output[index]);
		let exit_tx = transactions.iter().find(|tx| tx.compute_txid() == *exit_txid).unwrap().clone();
		assert_eq!(exit_tx.input.len(), 1);
		assert_eq!(exit_tx.input[0].previous_output, point);
		assert_eq!(exit_tx.output.len(), 2);
		assert_eq!(vtxo.point(), OutPoint::new(exit_tx.compute_txid(), 0));
		assert_eq!(exit_tx.output[0], vtxo.txout());
		assert!(exit_tx.output[1].is_p2a_fee_anchor());
		branches.push(Branch { checkpoint: point, exit_tx, output: vtxo.point() });
	}
	client.register_vtxo_transactions_with_server(&outputs).await.unwrap();
	(outputs, Node { input: input_point, checkpoint_tx, branches, payees })
}

#[tokio::test]
#[ignore = "327 native payees; run serially with a 900-second regtest slot"]
async fn expiry_direct_payout_above_ten_kilovbytes_pays_every_owner() {
	tokio::time::timeout(Duration::from_secs(900), direct_large_payout()).await
		.expect("large direct expiry payout exceeded 900 seconds");
}

async fn fund_sponsors(ctx: &TestContext, srv: &Captaind, count: usize) -> BTreeSet<OutPoint> {
	let address = srv.watchmand().wait_wallet_address().await;
	let mut sponsors = BTreeSet::new();
	for _ in 0..count {
		let txid = ctx.bitcoind().fund_addr(address.clone(), sat(200_000)).await;
		let tx = ctx.bitcoind().await_transaction(txid).await;
		let outputs = tx.output.iter().enumerate().filter(|(_, out)| out.script_pubkey == address.script_pubkey())
			.collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert_eq!(outputs[0].1.value, sat(200_000));
		sponsors.insert(OutPoint::new(txid, outputs[0].0 as u32));
	}
	assert_eq!(sponsors.len(), count);
	sponsors
}

async fn post_leaves(srv: &Captaind, owners: &[Owner], terminals: &[(usize, Vtxo<Full>)]) {
	let mut mailbox = srv.get_mailbox_public_rpc().await;
	let mailbox_pubkey = srv.ark_info().await.mailbox_pubkey;
	for (index, leaf) in terminals {
		let owner = &owners[*index];
		let blinded = owner.mailbox.to_blinded(mailbox_pubkey, &owner.key).unwrap();
		mailbox.post_arkoor_message(protos::mailbox_server::PostArkoorMessageRequest {
			blinded_id: blinded.as_ref().to_vec(), vtxos: vec![leaf.serialize()],
		}).await.unwrap();
	}
}

async fn split_leaves(
	srv: &Captaind, client: &bark::Wallet, rpc: &mut ArkClient, owners: &[Owner],
	mut jobs: VecDeque<(Vtxo<Full>, Keypair, Vec<usize>)>, bounded: bool,
) -> (Vec<Node>, Vec<(usize, Vtxo<Full>)>) {
	let mut nodes = Vec::new();
	let mut terminals = Vec::new();
	while let Some((input, key, group)) = jobs.pop_front() {
		if bounded {
			assert!(input.exit_depth() < 10);
			assert_eq!(group.len() % 4, 0);
		}
		let parts = group.len().min(4);
		let base = group.len() / parts;
		let extra = group.len() % parts;
		let mut offset = 0;
		let mut destinations = Vec::new();
		let mut children = Vec::new();
		for part in 0..parts {
			let count = base + usize::from(part < extra);
			let indices = group[offset..offset + count].to_vec();
			offset += count;
			let child_key = if count == 1 { owners[indices[0]].key }
				else { client.derive_store_next_keypair().await.unwrap().0 };
			destinations.push(ArkoorDestination {
				total_amount: sat(count as u64 * LEAF_SAT), policy: ark::VtxoPolicy::new_pubkey(child_key.public_key()),
			});
			children.push((indices, child_key));
		}
		let (outputs, node) = native_node(srv, client, rpc, input, key, destinations, group.len(), owners).await;
		nodes.push(node);
		for (output, (indices, child_key)) in outputs.into_iter().zip(children) {
			if bounded { assert!(output.exit_depth() <= 11); }
			if indices.len() == 1 {
				if bounded { assert_eq!(output.exit_depth(), 11); }
				terminals.push((indices[0], output));
			} else { jobs.push_back((output, child_key, indices)); }
		}
	}
	(nodes, terminals)
}

async fn direct_large_payout() {
	let ctx = TestContext::new("server/expiry_direct_payout_above_ten_kilovbytes").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().cfg(|cfg| { cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(4_320);
		cfg.vtxo_exit_delta = BlockDelta::new(144);
		cfg.max_vtxo_exit_depth = 100;
		cfg.max_arkoor_fanout = 4;
	}).watchmand().create().await;
	assert_eq!(srv.watchmand().config().watchman.progress_grace_period, BlockDelta::new(2));
	// Give every owner a separate random account and payout destination.
	let owners = (0..PAYEES).map(|_| owner()).collect::<Vec<_>>();
	assert_eq!(owners.iter().map(|o| o.script.clone()).collect::<BTreeSet<_>>().len(), PAYEES);
	// This lower bound uses real consensus serialization, without any input,
	// witness, drain, anchor or transaction-header overhead.
	let minimum_outputs = owners[..324].iter().map(|o|
		serialize(&TxOut { value: sat(LEAF_SAT - 2_000), script_pubkey: o.script.clone() }).len()
	).sum::<usize>();
	assert_eq!(minimum_outputs, 324 * 43);
	assert!(minimum_outputs > 10_000);
	let mut rpc = srv.get_public_rpc().await;
	let wallet = ctx.bark("source", &srv).cfg(|cfg| cfg.daemon_manual_sync = true)
		.funded(sat(2_000_000)).create().await;
	let ids = wallet.board_and_confirm_and_register(&ctx, sat(PAYEES as u64 * LEAF_SAT)).await;
	assert_eq!(ids.len(), 1);
	let client = wallet.client().await;
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let source = client.get_full_vtxo(ids[0]).await.unwrap();
	let source_key = client.get_vtxo_key(&source).await.unwrap();
	client.register_vtxo_transactions_with_server([&source]).await.unwrap();
	let _sponsor_funding = fund_sponsors(&ctx, &srv, 16).await;
	ctx.generate_blocks(2).await;

	let mut destinations = Vec::new();
	let mut jobs = VecDeque::new();
	let root_groups = [Vec::from_iter(0..324), vec![324], vec![325], vec![326]];
	let mut child_keys = Vec::new();
	for group in &root_groups {
		let key = if group.len() == 1 { owners[group[0]].key }
			else { client.derive_store_next_keypair().await.unwrap().0 };
		destinations.push(ArkoorDestination {
			total_amount: sat(group.len() as u64 * LEAF_SAT), policy: ark::VtxoPolicy::new_pubkey(key.public_key()),
		});
		child_keys.push((group.len() > 1).then_some(key));
	}
	let (outputs, root_node) = native_node(&srv, &client, &mut rpc, source.clone(), source_key, destinations, PAYEES, &owners).await;
	let mut terminals = Vec::new();
	for ((output, key), group) in outputs.into_iter().zip(child_keys).zip(root_groups) {
		if let Some(key) = key { jobs.push_back((output, key, group)); }
		else { terminals.push((group[0], output)); }
	}
	let (mut nodes, mut leaves) = split_leaves(&srv, &client, &mut rpc, &owners, jobs, false).await;
	nodes.insert(0, root_node);
	terminals.append(&mut leaves);
	assert_eq!(nodes.len(), 154);
	assert_eq!(terminals.len(), PAYEES);
	assert_eq!(terminals.iter().map(|(i, _)| *i).collect::<BTreeSet<_>>().len(), PAYEES);
	post_leaves(&srv, &owners, &terminals).await;
	drop(client);
	drop(wallet);
	drop(rpc);
	let current = ctx.bitcoind().get_block_count().await as u32;
	assert!(current < source.expiry_height().to_u32());
	let expired = ctx.generate_blocks(source.expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let terminal_ids = terminals.iter().map(|(_, leaf)| leaf.id().to_string()).collect::<Vec<_>>();
	let rows = tokio::time::timeout(Duration::from_secs(180), async {
		loop {
			srv.watchmand().trigger_sweep().await;
			let rows = db.read(async |tx| Ok(tx.query(
				"SELECT e.vtxo_id,c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat, \
				 c.spk,c.owner_mailbox,c.user_pubkey,a.sweep_tx,a.sweep_txid \
				 FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
				 JOIN expiry_allocation_state a ON a.id=c.allocation_id \
				 WHERE e.vtxo_id=ANY($1) AND a.status<>'conflicted' AND a.broadcast_at IS NOT NULL",
				&[&terminal_ids],
			).await?)).await.unwrap();
			if rows.len() == PAYEES { break rows; }
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
	}).await.expect("all owners must receive a direct payout decision");
	let txids = rows.iter().map(|row| row.get::<_, String>("sweep_txid")).collect::<BTreeSet<_>>();
	assert_eq!(txids.len(), 1, "one physical root needs one payout transaction");
	let payout = deserialize::<Transaction>(rows[0].get("sweep_tx")).unwrap();
	assert_eq!(payout.version, bitcoin::transaction::Version::TWO);
	assert!(payout.vsize() > 10_000 && payout.vsize() <= 100_000);
	assert!(payout.input.iter().any(|input| input.previous_output == source.chain_anchor()));
	assert!(actual_fee(&ctx, &payout).await > sat(0));
	let core = ctx.bitcoind().sync_client();
	let publication = core.custom_get_raw_transaction_info(payout.compute_txid(), None).unwrap().unwrap();
	assert_eq!(publication.txid, payout.compute_txid());
	let owner_scripts = owners.iter().map(|owner| owner.script.clone()).collect::<BTreeSet<_>>();
	let outputs = payout.output.iter().filter(|output| owner_scripts.contains(&output.script_pubkey))
		.collect::<Vec<_>>();
	assert_eq!(outputs.len(), PAYEES);
	for output in outputs { assert_eq!(output.value, sat(LEAF_SAT - 2_000)); }
	for (index, leaf) in &terminals {
		let row = rows.iter().find(|row| row.get::<_, String>("vtxo_id") == leaf.id().to_string()).unwrap();
		let owner = &owners[*index];
		assert_eq!(row.get::<_, i64>("gross_sat"), LEAF_SAT as i64);
		assert_eq!(row.get::<_, i64>("fee_sat"), 2_000);
		assert_eq!(row.get::<_, i64>("payout_sat"), (LEAF_SAT - 2_000) as i64);
		assert_eq!(row.get::<_, i64>("forfeited_sat"), 0);
		assert_eq!(row.get::<_, String>("spk"), owner.script.as_bytes().to_lower_hex_string());
		assert_eq!(row.get::<_, String>("owner_mailbox"), owner.mailbox.as_pubkey().to_string());
		assert_eq!(row.get::<_, String>("user_pubkey"), owner.key.public_key().to_string());
	}
	let splits = db.read(async |tx| Ok(tx.query_one(
		"SELECT COUNT(*) FROM expiry_split_request", &[],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(splits, 0, "direct payout must not create fork expiry splits");
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	for owner in &owners {
		let index = payout.output.iter().position(|output| output.script_pubkey == owner.script).unwrap();
		let utxo = core.get_tx_out(&payout.compute_txid(), index as u32, Some(true)).unwrap().unwrap();
		assert!(utxo.confirmations >= 6);
		assert_eq!(utxo.value, sat(LEAF_SAT - 2_000));
	}
}

/// Standard, client-owned leaves must not separate two payable recipient coins.
#[tokio::test]
async fn standard_padding_keeps_same_script_victim_payable() {
	const TINY: usize = 222;
	const SOURCE: u64 = 200_000;
	const PAD: u64 = 800;
	let ctx = TestContext::new("server/standard_padding_keeps_same_script_victim_payable").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(64);
		cfg.vtxo_exit_delta = BlockDelta::new(2);
		cfg.max_vtxo_exit_depth = 100;
		cfg.max_arkoor_fanout = 4;
	}).watchmand().create().await;
	let mut source_wallets = Vec::new();
	for index in 0..3 {
		source_wallets.push(ctx.bark(format!("source_{index}"), &srv)
			.cfg(|cfg| cfg.daemon_manual_sync = true).funded(sat(300_000)).create().await);
	}
	for wallet in &source_wallets { wallet.board(sat(SOURCE)).await; }
	ctx.generate_blocks(BOARD_CONFIRMATIONS).await;
	let mut clients = Vec::new();
	let mut sources = Vec::new();
	for wallet in &source_wallets {
		wallet.sync().await;
		let client = wallet.client().await;
		let spendable = client.spendable_vtxos().await.unwrap();
		assert_eq!(spendable.len(), 1);
		sources.push(client.get_full_vtxo(spendable[0].id()).await.unwrap());
		clients.push(client);
	}
	assert!(sources.iter().all(|source| source.amount() == sat(SOURCE)
		&& source.expiry_height() == sources[0].expiry_height()));
	let mut order = [0usize, 1, 2];
	order.sort_by_key(|index| ark::VtxoId::from(sources[*index].chain_anchor()));
	let attacker_index = order[1];
	let honest_indices = [order[0], order[2]];
	let recipient = ctx.bark("victim_recipient", &srv).create().await;
	let recipient_client = recipient.client().await;
	let victim_mailbox_key = recipient_client.mailbox_keypair();
	let victim_mailbox = MailboxIdentifier::from_pubkey(victim_mailbox_key.public_key());
	let victim_script = ScriptBuf::new_p2tr(&SECP, victim_mailbox_key.x_only_public_key().0, None);
	let mut owners = (0..TINY + 1).map(|_| owner()).collect::<Vec<_>>();
	let attacker_change_index = TINY;
	let victim_indices = [owners.len(), owners.len() + 1];
	for _ in 0..2 {
		let key = recipient_client.derive_store_next_keypair().await.unwrap().0;
		owners.push(Owner {
			key, mailbox: victim_mailbox,
			script: victim_script.clone(),
		});
	}
	let honest_change_indices = [owners.len(), owners.len() + 1];
	owners.push(owner());
	owners.push(owner());
	let mut rpc = srv.get_public_rpc().await;
	let mut victim_leaves = Vec::new();
	for (position, index) in honest_indices.into_iter().enumerate() {
		let key = clients[index].get_vtxo_key(&sources[index]).await.unwrap();
		let destinations = [
			ArkoorDestination { total_amount: sat(1_200),
				policy: ark::VtxoPolicy::new_pubkey(owners[victim_indices[position]].key.public_key()) },
			ArkoorDestination { total_amount: sat(SOURCE - 1_200),
				policy: ark::VtxoPolicy::new_pubkey(owners[honest_change_indices[position]].key.public_key()) },
		].to_vec();
		let (outputs, _) = native_node(&srv, &clients[index], &mut rpc,
			sources[index].clone(), key, destinations, 2, &owners).await;
		assert_eq!(outputs.len(), 2);
		assert_eq!(outputs[0].amount(), sat(1_200));
		victim_leaves.push((victim_indices[position], outputs[0].clone()));
	}
	post_leaves(&srv, &owners, &victim_leaves).await;
	recipient.sync().await;
	assert_eq!(recipient.spendable_balance().await, sat(2_400));

	let attacker_key = clients[attacker_index].get_vtxo_key(&sources[attacker_index]).await.unwrap();
	let tiny_branch_key = clients[attacker_index].derive_store_next_keypair().await.unwrap().0;
	let destinations = [
		ArkoorDestination { total_amount: sat(TINY as u64 * PAD),
			policy: ark::VtxoPolicy::new_pubkey(tiny_branch_key.public_key()) },
		ArkoorDestination { total_amount: sat(SOURCE - TINY as u64 * PAD),
			policy: ark::VtxoPolicy::new_pubkey(owners[attacker_change_index].key.public_key()) },
	].to_vec();
	let (first, _) = native_node(&srv, &clients[attacker_index], &mut rpc,
		sources[attacker_index].clone(), attacker_key, destinations, TINY + 1, &owners).await;
	assert_eq!(first.len(), 2);
	let mut jobs = VecDeque::from([(first[0].clone(), tiny_branch_key, (0..TINY).collect::<Vec<_>>())]);
	let mut tiny_leaves = Vec::new();
	while let Some((input, key, group)) = jobs.pop_front() {
		let parts = group.len().min(4);
		let base = group.len() / parts;
		let extra = group.len() % parts;
		let mut offset = 0;
		let mut destinations = Vec::new();
		let mut children = Vec::new();
		for part in 0..parts {
			let count = base + usize::from(part < extra);
			let indices = group[offset..offset + count].to_vec();
			offset += count;
			let child_key = if count == 1 { owners[indices[0]].key }
				else { clients[attacker_index].derive_store_next_keypair().await.unwrap().0 };
			destinations.push(ArkoorDestination { total_amount: sat(count as u64 * PAD),
				policy: ark::VtxoPolicy::new_pubkey(child_key.public_key()) });
			children.push((indices, child_key));
		}
		let (outputs, _) = native_node(&srv, &clients[attacker_index], &mut rpc,
			input, key, destinations, group.len(), &owners).await;
		for (output, (indices, child_key)) in outputs.into_iter().zip(children) {
			if indices.len() == 1 {
				assert_eq!(output.amount(), sat(PAD));
				tiny_leaves.push(output);
			} else { jobs.push_back((output, child_key, indices)); }
		}
	}
	assert_eq!(tiny_leaves.len(), TINY);
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let victim_ids = victim_leaves.iter().map(|(_, leaf)| leaf.id().to_string()).collect::<Vec<_>>();
	let _sponsors = fund_sponsors(&ctx, &srv, 8).await;
	ctx.generate_blocks(2).await;
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(sources[0].expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let mut final_rows = None;
	for _ in 0..40 {
		srv.watchmand().trigger_sweep().await;
		let rows = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id,c.allocation_id,c.gross_sat,c.payout_sat,c.forfeited_sat, \
			a.status::TEXT AS status,a.sweep_tx FROM expiry_entitlement e \
			JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
			JOIN expiry_allocation_state a ON a.id=c.allocation_id \
			WHERE e.vtxo_id=ANY($1) AND a.status<>'conflicted'",
			&[&victim_ids]).await?)).await.unwrap();
		if rows.len() == 2 && rows.iter().all(|row| row.get::<_, &str>("status") == "confirmed") {
			final_rows = Some(rows);
			break;
		}
		if !ctx.bitcoind().sync_client().get_raw_mempool().unwrap().is_empty() {
			let height = ctx.generate_blocks(1).await;
			srv.wait_for_sync_height(height).await;
			srv.watchmand().wait_for_sync_height(height).await;
		} else {
			tokio::time::sleep(Duration::from_millis(200)).await;
		}
	}
	let rows = final_rows.expect("both victim leaves did not settle in 40 blocks");
	let allocations = rows.iter().map(|row| row.get::<_, i64>("allocation_id")).collect::<BTreeSet<_>>();
	assert_eq!(allocations.len(), 1, "both victim leaves must share one physical payout");
	assert_eq!(rows.iter().map(|row| row.get::<_, i64>("gross_sat")).sum::<i64>(), 2_400);
	assert_eq!(rows.iter().map(|row| row.get::<_, i64>("payout_sat")).sum::<i64>(), 400);
	assert!(rows.iter().all(|row| row.get::<_, i64>("forfeited_sat") == 0));
	let parent = deserialize::<Transaction>(rows[0].get::<_, &[u8]>("sweep_tx")).unwrap();
	let payout = parent.output.iter().find(|output| output.script_pubkey == victim_script)
		.expect("victim output missing");
	assert_eq!(payout.value, sat(400));
	assert_eq!(parent.version, bitcoin::transaction::Version::TWO);
	assert!(actual_fee(&ctx, &parent).await > sat(0));
	assert!(ctx.bitcoind().sync_client().custom_get_raw_transaction_info(parent.compute_txid(), None)
		.unwrap().unwrap().confirmations.unwrap_or(0) > 0);
}
