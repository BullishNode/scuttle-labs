//! A five-block grant-closure reorg before the sender refund lets the
//! rightful receiver collect once, without paying its sender as well.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::PaymentHash;
use ark::vtxo::policy::signing::VtxoSigner;
use ark::vtxo::Full;
use ark::{ProtocolEncoding, Vtxo, VtxoPolicy};
use ark_testing::{btc, sat, TestContext};
use bitcoin::consensus::{deserialize, encode::serialize_hex};
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::{OutPoint, Transaction, TxIn, TxOut, Txid, Witness};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use bitcoin_ext::{BlockDelta, BlockHeight};
use server::database::Db;
use server_rpc::protos;

#[tokio::test]
async fn expiry_internal_reclaim_reorg_five_blocks_before_refund_pays_receiver_once() {
	tokio::time::timeout(Duration::from_secs(420), reclaim_reorg()).await
		.expect("reclaim reorg scenario exceeded its budget");
}

async fn customer_allocations(db: &Db, source: &str, status: &str) -> Vec<(i64, Transaction)> {
	db.read(async |t| Ok(t.query("SELECT a.id,a.sweep_tx FROM expiry_allocation_state a \
		JOIN expiry_allocation_coin c ON c.allocation_id=a.id JOIN expiry_entitlement e ON e.id=c.entitlement_id \
		WHERE e.vtxo_id=$1 AND c.disposition='customer' AND a.status=$2 AND a.sweep_tx IS NOT NULL",
		&[&source, &status]).await?.into_iter()
		.map(|row| (row.get::<_, i64>(0), deserialize(row.get::<_, &[u8]>(1)).unwrap())).collect())).await.unwrap()
}

async fn rebuilt_current_view(ctx: &TestContext, db: &Db) -> bool {
	let chain = ctx.bitcoind().sync_client();
	let height = BlockHeight::new(u32::try_from(chain.get_block_count().unwrap()).unwrap());
	let hash = chain.get_block_hash(u64::from(height.to_u32())).unwrap();
	db.read(async |t| {
		let Some(view) = t.expiry_view().await? else { return Ok(false); };
		Ok((t.expiry_view().await? == Some(view))
			&& view.height == height && view.hash == hash)
	}).await.unwrap()
}

/// Inspect the complete current non-coinbase graph, not orphaned receipts or
/// database status alone. Its boundary consists of external prevouts.
async fn assert_canonical_money(
	ctx: &TestContext, sender: &bitcoin::Script, receiver: &bitcoin::Script,
	sender_amount: bitcoin::Amount, receiver_amount: bitcoin::Amount,
) -> BTreeMap<Txid, Transaction> {
	let chain = ctx.bitcoind().sync_client();
	let mut transactions = BTreeMap::new();
	for height in 0..=chain.get_block_count().unwrap() {
		let block = chain.get_block(&chain.get_block_hash(height).unwrap()).unwrap();
		for tx in block.txdata.into_iter().skip(1) {
			assert!(transactions.insert(tx.compute_txid(), tx).is_none());
		}
	}
	let mut consumed = BTreeSet::new();
	let mut boundary = BTreeMap::new();
	let mut fees = sat(0);
	for tx in transactions.values() {
		let mut input_value = sat(0);
		for input in &tx.input {
			assert!(consumed.insert(input.previous_output), "canonical transactions cannot reuse a physical input");
			let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
			let output = previous.output[input.previous_output.vout as usize].clone();
			input_value += output.value;
			if !transactions.contains_key(&input.previous_output.txid) {
				assert!(boundary.insert(input.previous_output, output).is_none());
			}
		}
		let output_value = tx.output.iter().map(|output| output.value).sum::<bitcoin::Amount>();
		fees += input_value.checked_sub(output_value).expect("a canonical transaction cannot create value");
	}
	let mut terminal = sat(0);
	let mut sender_outputs = Vec::new();
	let mut receiver_outputs = Vec::new();
	for (txid, tx) in &transactions {
		for (vout, output) in tx.output.iter().enumerate() {
			let point = OutPoint::new(*txid, vout as u32);
			if output.script_pubkey.as_script() == sender { sender_outputs.push((point, output)); }
			if output.script_pubkey.as_script() == receiver { receiver_outputs.push((point, output)); }
			if consumed.contains(&point) { continue; }
			// Ignore mempool spenders: this is the canonical replacement cut.
			if output.value != sat(0) && !output.script_pubkey.is_op_return() {
				let actual = chain.get_tx_out(txid, vout as u32, Some(false)).unwrap().unwrap();
				assert_eq!(actual.value, output.value);
				assert_eq!(actual.script_pub_key.script().unwrap(), output.script_pubkey);
			}
			terminal += output.value;
		}
	}
	assert_eq!(boundary.values().map(|output| output.value).sum::<bitcoin::Amount>(), terminal + fees,
		"the complete current graph's physical inputs must equal terminal outputs and actual miner fees");
	for (outputs, amount) in [(sender_outputs, sender_amount), (receiver_outputs, receiver_amount)] {
		assert_eq!(outputs.len(), if amount == sat(0) { 0 } else { 1 }, "exactly the rightful party must have one canonical payment");
		if let Some((point, output)) = outputs.first() {
			assert_eq!(output.value, amount);
			let actual = chain.get_tx_out(&point.txid, point.vout, Some(false)).unwrap().unwrap();
			assert!(actual.confirmations >= 6, "the actual rightful payment must be six confirmations deep");
		}
	}
	transactions
}

async fn reclaim_reorg() {
	let ctx = TestContext::new("server/expiry_internal_reclaim_reorg_five_before_refund").await;
	let lightning = ctx.new_lightning_setup_no_channel("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).bitcoind(ctx.bitcoind_arc())
		.funded(btc(10)).cfg(|cfg| { cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(24);
			cfg.vtxopool.vtxo_lifetime = BlockDelta::new(24);
			cfg.vtxopool.vtxo_pre_expiry = BlockDelta::new(2);
			cfg.vtxopool.vtxo_targets = vec![server::vtxopool::VtxoTarget { amount: sat(50_000), count: 4 }];
			cfg.invoice_check_interval = Duration::from_secs(1);
		}).watchmand().create().await;
	srv.wait_for_vtxopool(&ctx).await;
	// The receiver keeps its wallet: it will come back with the preimage.
	let recipient = ctx.bark_sdk("recipient", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let invoice = recipient.bolt11_invoice(sat(90_000), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	let receiver_key = recipient.new_address().await.unwrap().policy().user_pubkey();
	let receiver_destination = recipient.fallback_script();
	let sender = ctx.bark("absent-sender", &srv).funded(sat(200_000)).create().await;
	let ids = sender.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let client = sender.client().await;
	let input = client.get_full_vtxo(ids[0]).await.unwrap();
	let key = client.get_vtxo_key(&input).await.unwrap();
	let destination = client.fallback_script();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let incoming_deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let outgoing_deadline = tip + BlockDelta::new(36);
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input],
		VtxoPolicy::new_server_htlc_send(key.public_key(), hash, incoming_deadline)).unwrap()
		.generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
	}).await.unwrap().into_inner();
	let incoming = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
		.unwrap().build_signed_vtxos();
	client.register_vtxo_transactions_with_server(&incoming).await.unwrap();
	let source = incoming[0].id().to_string();
	lightning.sync().await;
	rpc.initiate_lightning_payment(protos::InitiateLightningPaymentRequest {
		invoice: invoice.to_string(), payment_amount_sat: 90_000,
		htlc_vtxo_ids: vec![incoming[0].id().to_bytes().to_vec()], mailbox_id: None, retry_for_secs: Some(5),
	}).await.unwrap();
	let granted = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		owner_proof: Some(recipient.owner_link(receiver_key).await.unwrap().into()),
		payment_hash: hash.to_vec(), user_pubkey: receiver_key.serialize().to_vec(),
		htlc_recv_expiry: outgoing_deadline.to_u32(), lightning_receive_anti_dos: None,
	}).await.unwrap().into_inner();
	let grants = granted.htlc_vtxos.iter().map(|b| Vtxo::<Full>::deserialize(b).unwrap()).collect::<Vec<_>>();
	assert_eq!(grants.iter().map(|g| g.amount()).sum::<bitcoin::Amount>(), sat(90_000));
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	drop(client); drop(rpc);
	// Expose the grant leaves so the receiver could claim them on chain.
	let mut seen = BTreeSet::new();
	for grant in &grants { super::expiry_internal_partial_terminal::confirm_actual_path(&ctx, grant, &mut seen).await; }
	let fee_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(fee_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let chain = ctx.bitcoind().sync_client();

	// Stop at the actual five-deep grant closures, before any sender refund.
	let reclaims: BTreeSet<Txid> = tokio::time::timeout(Duration::from_secs(200), async {
		loop {
			let height = ctx.generate_blocks(1).await;
			srv.wait_for_sync_height(height).await;
			srv.watchmand().wait_for_sync_height(height).await;
			tokio::time::sleep(Duration::from_secs(1)).await;
			let closures = super::expiry_lightning_terminal::confirmed_grant_closures(
				&ctx, &db, &grants, &receiver_destination,
			).await;
			let required = closures.as_ref().map(|rows| rows.values().map(|(_, closed)|
				closed.checked_add(6).unwrap()).max().unwrap());
			if required.is_none_or(|required| height.to_u32() < required) {
				let exact_h5 = required == Some(height.to_u32() + 1);
				if exact_h5 {
					while !rebuilt_current_view(&ctx, &db).await {
						tokio::time::sleep(Duration::from_millis(50)).await;
					}
				}
				let count = db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c \
					JOIN expiry_entitlement e ON e.id=c.entitlement_id \
					WHERE e.vtxo_id=$1 AND c.disposition='customer'", &[&source]).await?.get::<_, i64>(0))).await.unwrap();
				assert_eq!(count, 0, "sender allocation must wait through H+5 for every actual grant closure");
			}
			let Some(closures) = closures else { continue; };
			let reclaims = closures.values().map(|(txid, _)| *txid).collect::<BTreeSet<Txid>>();
			let depths = reclaims.iter().map(|txid| chain.custom_get_raw_transaction_info(*txid, None).unwrap()
				.unwrap().confirmations.unwrap()).collect::<Vec<_>>();
			assert!(depths.iter().all(|depth| *depth > 0 && *depth <= 6));
			if depths.iter().any(|depth| *depth != 5) { continue; }
			break reclaims;
		}
	}).await.expect("grants were not reclaimed");

	// The reorg: drop the earliest block holding a reclaim.
	let reclaim_heights = reclaims.iter().map(|txid| {
		let info = chain.custom_get_raw_transaction_info(*txid, None).unwrap().unwrap();
		chain.get_block_header_info(&info.blockhash.unwrap()).unwrap().height
	}).collect::<Vec<_>>();
	let old_tip = chain.get_block_count().unwrap() as usize;
	let old_tip_hash = chain.get_block_hash(old_tip as u64).unwrap();
	assert_eq!(chain.get_block_header_info(&old_tip_hash).unwrap().height, old_tip);
	let fork_height = *reclaim_heights.iter().min().unwrap();
	let fork_depth = old_tip + 1 - fork_height;
	let ancestor_height = fork_height.checked_sub(1).unwrap();
	let ancestor_hash = chain.get_block_hash(ancestor_height as u64).unwrap();
	let removed_hashes = (fork_height..=old_tip).map(|height| chain.get_block_hash(height as u64).unwrap()).collect::<Vec<_>>();
	assert!((1..=5).contains(&fork_depth), "supported grant-closure reorg must stay within five blocks");
	assert_eq!(fork_depth, 5);
	// Replay block by block so every transaction keeps a final locktime.
	let mut replay = Vec::<Vec<Transaction>>::new();
	let mut excluded = reclaims.clone();
	for height in fork_height..=old_tip {
		let block = chain.get_block(&chain.get_block_hash(height as u64).unwrap()).unwrap();
		let mut kept = Vec::new();
		for tx in block.txdata.into_iter().skip(1) {
			if excluded.contains(&tx.compute_txid())
				|| tx.input.iter().any(|i| excluded.contains(&i.previous_output.txid))
			{
				excluded.insert(tx.compute_txid());
				continue;
			}
			kept.push(tx);
		}
		replay.push(kept);
	}
	let miner = ctx.bitcoind().get_new_address();
	srv.watchmand().stop().await.unwrap();
	srv.stop().await.unwrap();
	chain.invalidate_block(&chain.get_block_hash(fork_height as u64).unwrap()).unwrap();
	assert_eq!(chain.get_block_count().unwrap(), ancestor_height as u64, "Core must remove the exact selected number of blocks");
	assert_eq!(chain.get_block_hash(ancestor_height as u64).unwrap(), ancestor_hash, "the selected common ancestor must survive exactly");
	assert_eq!(old_tip - chain.get_block_count().unwrap() as usize, fork_depth);
	for hash in &removed_hashes {
		assert!(chain.get_block_header_info(hash).unwrap().confirmations < 0, "every selected old block must actually be orphaned");
	}
	let chain = ctx.bitcoind().sync_client();
	let is_confirmed = |txid: Txid| chain.custom_get_raw_transaction_info(txid, None).unwrap()
		.is_some_and(|info| info.confirmations.is_some_and(|d| d > 0));
	let generate = |txs: &[Transaction]| -> serde_json::Value {
		chain.call("generateblock", &[serde_json::json!(miner.to_string()),
			serde_json::json!(txs.iter().map(serialize_hex).collect::<Vec<_>>())]).unwrap()
	};
	for (offset, txs) in replay.iter().enumerate() {
		generate(txs);
		let height = fork_height + offset;
		assert_eq!(chain.get_block_count().unwrap(), height as u64);
		assert_ne!(chain.get_block_hash(height as u64).unwrap(), removed_hashes[offset],
			"replacement mining must not accidentally restore an old removed block");
	}
	assert_eq!(chain.get_block_hash(ancestor_height as u64).unwrap(), ancestor_hash);

	// The receiver comes back and claims every grant with the preimage.
	let mut claim = Transaction {
		version: bitcoin::transaction::Version::TWO,
		lock_time: bitcoin::absolute::LockTime::ZERO,
		input: Vec::new(),
		output: vec![TxOut { value: sat(90_000 - 2_000), script_pubkey: receiver_destination.clone() }],
	};
	let clauses = grants.iter().map(|g| g.policy().as_server_htlc_recv().unwrap()
		.user_reveals_preimage_clause(g.exit_delta())).collect::<Vec<_>>();
	for (grant, clause) in grants.iter().zip(&clauses) {
		claim.input.push(TxIn { previous_output: grant.point(), script_sig: Default::default(),
			sequence: clause.sequence(), witness: Witness::new() });
	}
	let prevouts = grants.iter().map(Vtxo::txout).collect::<Vec<_>>();
	for (index, (grant, clause)) in grants.iter().zip(&clauses).enumerate() {
		claim.input[index].witness = recipient.sign_input_with_clause(grant, &clause.clone().into(), index,
			&mut SighashCache::new(&claim), &Prevouts::All(&prevouts)).await.unwrap();
	}
	// The receiver can only claim grants that are unspent on the new branch.
	let claimable = grants.iter().all(|g| chain.get_tx_out(&g.point().txid, g.point().vout, Some(false)).unwrap().is_some());
	assert!(claimable, "the pre-refund reorg must leave the receiver grants claimable");
	// Mine empty blocks until the receiver's relative timelock allows the claim.
	for _ in 0..200 {
		if chain.test_mempool_accept(&[&claim]).unwrap()[0].allowed { break; }
		generate(&[]);
	}
	generate(std::slice::from_ref(&claim));
	assert!(is_confirmed(claim.compute_txid()), "the receiver's preimage claim must confirm");
	srv.start().await.unwrap();
	srv.watchmand().start().await.unwrap();

	// Let the server see the new branch; keep mining for a while.
	for _ in 0..20 {
		let height = ctx.generate_blocks(1).await;
		srv.wait_for_sync_height(height).await;
		srv.watchmand().wait_for_sync_height(height).await;
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
	let sender_refunded = customer_allocations(&db, &source, "confirmed").await.iter()
		.any(|(_, tx)| is_confirmed(tx.compute_txid()) && tx.output.iter().any(|o| o.script_pubkey == destination));
	let receiver_paid = is_confirmed(claim.compute_txid());
	assert!(!(sender_refunded && receiver_paid),
		"DOUBLE PAYMENT: the sender's refund and the receiver's preimage claim both confirmed");
	assert!(receiver_paid && !sender_refunded, "a supported pre-refund fork must actually pay its receiver, not strand both sides");
	let sender_decisions = db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c \
		JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1 AND c.disposition='customer'",
		&[&source]).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(sender_decisions, 0, "the receiver's actual collection cannot also authorize a sender refund");
	let current = assert_canonical_money(&ctx, &destination, &receiver_destination, sat(0), sat(88_000)).await;
	assert_eq!(current.get(&claim.compute_txid()), Some(&claim));
	assert_eq!(prevouts.iter().map(|output| output.value).sum::<bitcoin::Amount>(), sat(90_000));
	assert_eq!(claim.output.iter().map(|output| output.value).sum::<bitcoin::Amount>(), sat(88_000));
	assert_eq!(prevouts.iter().map(|output| output.value).sum::<bitcoin::Amount>()
		- claim.output.iter().map(|output| output.value).sum::<bitcoin::Amount>(), sat(2_000));
}
