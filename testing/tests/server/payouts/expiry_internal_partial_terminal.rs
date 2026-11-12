//! Current-protocol partial-terminal recovery and exact signed-claim replay.
//! One attempt uses TWO original inputs and one complete receive set.

use std::collections::BTreeSet;
use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::PaymentHash;
use ark::vtxo::Full;
use ark::vtxo::policy::signing::VtxoSigner;
use ark::{ProtocolEncoding, Vtxo, VtxoId, VtxoPolicy};
use ark_testing::{btc, sat, TestContext};
use bitcoin::{Transaction, TxIn, TxOut, Witness};
use bitcoin::consensus::deserialize;
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use ark_testing::Captaind;
use server::database::Db;
use server_rpc::protos;

pub(super) use super::expiry_test::confirm_actual_path;

/// The A claim is an ordinary rightful timeout receipt, made while its owner
/// is present. Both actors then disappear; B must not be stranded. There is no
/// incorrect payout request, SQL money mutation, or fabricated resolution.
async fn partial_terminal() {
	let ctx = TestContext::new("server/expiry_internal_one_attempt_partial_terminal").await;
	let lightning = ctx.new_lightning_setup_no_channel("lightningd").await;
	let srv = Box::pin(ctx.captaind("server").lightningd(&lightning.internal).bitcoind(ctx.bitcoind_arc())
		.funded(btc(10)).cfg(|cfg| { cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(24);
			cfg.vtxopool.vtxo_lifetime = BlockDelta::new(24);
			cfg.vtxopool.vtxo_pre_expiry = BlockDelta::new(2);
			cfg.vtxopool.vtxo_targets = vec![server::vtxopool::VtxoTarget { amount: sat(50_000), count: 8 }];
			cfg.invoice_check_interval = Duration::from_secs(1);
		}).watchmand().create()).await;
	srv.wait_for_vtxopool(&ctx).await;
	let recipient = Box::pin(ctx.bark_sdk("recipient", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create()).await;
	let invoice = recipient.bolt11_invoice(sat(180_000), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	let receive_keypair = super::receive_claim_keypair(&srv, &recipient, hash).await;
	let receive_key = receive_keypair.public_key();
	let receive_destination = recipient.fallback_script();
	let actor = ctx.bark("sender", &srv).cfg(|cfg| cfg.daemon_manual_sync = true)
		.funded(sat(400_000)).create().await;
	let first = actor.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let second = actor.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	assert_eq!(first.len(), 1);
	assert_eq!(second.len(), 1);
	let sender = actor.client().await;
	let inputs = vec![sender.get_full_vtxo(first[0]).await.unwrap(), sender.get_full_vtxo(second[0]).await.unwrap()];
	assert_ne!(inputs[0].chain_anchor(), inputs[1].chain_anchor());
	assert!(inputs.iter().all(|input| input.amount() == sat(100_000)));
	let keys = vec![sender.get_vtxo_key(&inputs[0]).await.unwrap(), sender.get_vtxo_key(&inputs[1]).await.unwrap()];
	let fallback = sender.fallback_script();
	assert_ne!(fallback, receive_destination);
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let incoming_deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints(inputs,
		VtxoPolicy::new_server_htlc_send(keys[0].public_key(), hash, incoming_deadline)).unwrap()
		.generate_user_nonces(&keys).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let reply = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: super::signed_package(&srv, &[&sender], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
	}).await.unwrap().into_inner();
	let sources = builder.user_cosign(&keys, ArkoorPackageCosignResponse::try_from(reply).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(sources.len(), 2);
	assert!(sources.iter().all(|source| source.amount() == sat(100_000)));
	sender.register_vtxo_transactions_with_server(&sources).await.unwrap();
	lightning.sync().await;
	rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
		protos::InitiateLightningPaymentRequest {
		invoice: invoice.to_string(), payment_amount_sat: 180_000,
		htlc_vtxo_ids: sources.iter().map(|source| source.id().to_bytes().to_vec()).collect(),
		mailbox_id: None, retry_for_secs: Some(5),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	}, &keys[0]).await).await.unwrap();
	let reply = rpc.prepare_lightning_receive_claim(protos::PrepareLightningReceiveClaimRequest {
		payment_hash: hash.to_vec(), user_pubkey: receive_key.serialize().to_vec(),
		htlc_recv_expiry: (tip + BlockDelta::new(36)).to_u32(), lightning_receive_anti_dos: None,
		claim_attestation: Some(ark::attestations::LightningReceiveClaimAttestation::new(hash, tip + BlockDelta::new(36), &receive_keypair).serialize()),
	}).await.unwrap().into_inner();
	let grants = reply.htlc_vtxos.iter().map(|raw| Vtxo::<Full>::deserialize(raw).unwrap()).collect::<Vec<_>>();
	assert_eq!(grants.iter().map(Vtxo::amount).sum::<bitcoin::Amount>(), sat(180_000));
	assert!(grants.len() >= 2);
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let scope = db.read(async |t| {
		let rows = t.query("SELECT a.id,a.lightning_htlc_subscription_id AS subscription_id FROM lightning_payment_attempt a \
			WHERE a.payment_hash=$1 AND a.lightning_htlc_subscription_id IS NOT NULL", &[&hash.to_string()]).await?;
		assert_eq!(rows.len(), 1, "one actual funding attempt must own this receive subscription");
		let attempt: i64 = rows[0].get("id");
		let subscription: i64 = rows[0].get("subscription_id");
		let originals = t.query("SELECT vtxo_id FROM lightning_payment_attempt_htlc_vtxo \
			WHERE lightning_payment_attempt_id=$1 ORDER BY vtxo_id", &[&attempt]).await?
			.into_iter().map(|row| row.get::<_, String>("vtxo_id")).collect::<BTreeSet<_>>();
		let issued = t.query("SELECT v.vtxo_id FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id \
			WHERE v.lightning_htlc_subscription_id=$1 AND h.direction='outgoing' ORDER BY v.vtxo_id", &[&subscription]).await?
			.into_iter().map(|row| row.get::<_, String>("vtxo_id")).collect::<BTreeSet<_>>();
		Ok((originals, issued))
	}).await.unwrap();
	assert_eq!(scope.0, sources.iter().map(|source| source.id().to_string()).collect());
	assert_eq!(scope.1, grants.iter().map(|grant| grant.id().to_string()).collect());
	// Preserve every grant. No receiver claim, preimage or returning
	// wallet action is used to discharge the receive side.
	let fee_address = srv.watchmand().wait_wallet_address().await;
	srv.watchmand().stop().await.unwrap();
	let mut seen = BTreeSet::new();
	for grant in &grants { confirm_actual_path(&ctx, grant, &mut seen).await; }
	confirm_actual_path(&ctx, &sources[0], &mut seen).await;
	let unpaid_backing = sources[1].chain_anchor();
	let chain = ctx.bitcoind().sync_client();
	let point = sources[0].point();
	let current = ctx.bitcoind().get_block_count().await as u32;
	let confirmed = chain.get_tx_out(&point.txid, point.vout, Some(true)).unwrap().unwrap();
	let clause = sources[0].policy().as_server_htlc_send().unwrap()
		.user_claim_after_expiry_clause(sources[0].exit_delta());
	let csv = current + 1 - u32::try_from(confirmed.confirmations).unwrap() + clause.block_delta.to_u32();
	let mature = incoming_deadline.to_u32().max(csv) + 1;
	if mature > current { ctx.generate_blocks(mature - current).await; }
	// The central funding wallet must not reuse this customer receipt for later fee sponsors.
	let claim_address = actor.get_onchain_address().await;
	let mut claim = Transaction {
		version: bitcoin::transaction::Version::TWO, lock_time: clause.locktime(),
		input: vec![TxIn { previous_output: point, script_sig: Default::default(), sequence: clause.sequence(), witness: Witness::new() }],
		output: vec![TxOut { value: sat(98_000), script_pubkey: claim_address.script_pubkey() }],
	};
	let prevouts = [sources[0].txout()];
	let witness = sender.sign_input_with_clause(&sources[0], &clause.clone().into(), 0,
		&mut SighashCache::new(&claim), &Prevouts::All(&prevouts)).await.unwrap();
	claim.input[0].witness = witness;
	chain.send_raw_transaction(&claim).unwrap();
	let unspent = chain.get_tx_out(&unpaid_backing.txid,unpaid_backing.vout,Some(true)).unwrap().unwrap();
	assert!(unspent.confirmations > 0);
	assert_eq!(unspent.value,sources[1].amount());
	for grant in &grants {
		assert!(chain.get_tx_out(&grant.point().txid, grant.point().vout, Some(true)).unwrap().is_some());
	}
	ctx.generate_blocks(6).await;
	assert!(chain.get_tx_out(&claim.compute_txid(), 0, Some(true)).unwrap().unwrap().confirmations >= 6);
	assert_eq!(claim.output[0].value, sources[0].amount() - sat(2_000));
	drop(sender);
	drop(recipient);
	drop(rpc);
	for _ in 0..4 { ctx.bitcoind().fund_addr(fee_address.clone(), sat(150_000)).await; }
	ctx.generate_blocks(2).await;
	srv.watchmand().start().await.unwrap();
	// No new block is mined until this canonical A authorization has been
	// retained. Thus every G still exists on chain even if a parent is
	// already in the mempool; recording a txid alone cannot satisfy this.
	tokio::time::timeout(Duration::from_secs(30),async {
		loop {
			if assert_retained_claim(&db,sources[0].id(),&claim).await { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("retain the actual A raw body and source link before consuming G");
	for grant in &grants {
		assert!(chain.get_tx_out(&grant.point().txid,grant.point().vout,Some(false)).unwrap().is_some(),
			"the exact raw A authorization must be durable before canonical grant consumption");
	}
	let prior_fences = db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM ( \
		SELECT vtxo_id FROM expiry_entitlement UNION ALL SELECT vtxo_id FROM expiry_allocation_input \
		) prior WHERE vtxo_id=$1",&[&sources[0].id().to_string()]).await?.get::<_,i64>(0))).await.unwrap();
	assert_eq!(prior_fences,0,"this case qualifies an observed user claim without a prior decided/allocation fence");
	let unpaid = sources[1].id();
	let parent: Transaction = tokio::time::timeout(Duration::from_secs(150), async {
		for _ in 0..100 {
			let height = ctx.generate_blocks(1).await;
			srv.wait_for_sync_height(height).await;
			srv.watchmand().wait_for_sync_height(height).await;
			tokio::time::sleep(Duration::from_secs(1)).await;
			let row = db.read(async |t| Ok(t.query_opt("SELECT a.sweep_tx FROM expiry_allocation_state a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND a.status<>'conflicted' AND a.broadcast_at IS NOT NULL", &[&unpaid.to_string()]).await?)).await.unwrap();
			if let Some(row) = row { return deserialize(row.get("sweep_tx")).unwrap(); }
		}
		panic!("the genuinely unpaid original source still requires an absent user");
	}).await.expect("partial terminal closure must not strand the other original source");
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	super::expiry_lightning_terminal::assert_grants_reclaimed(&ctx, &db, &grants, &receive_destination).await;
	let mut unpaid_path = sources[1].transactions().map(|step|
		bitcoin::OutPoint::new(step.tx.compute_txid(), step.output_idx as u32)).collect::<BTreeSet<_>>();
	unpaid_path.insert(sources[1].chain_anchor());
	assert!(parent.input.iter().any(|input| unpaid_path.contains(&input.previous_output)),
		"a grant-funded parent alone cannot discharge B while its original backing remains independently live");
	assert!(!parent.input.iter().any(|input| input.previous_output == point));
	let (index, output) = parent.output.iter().enumerate().find(|(_, output)| output.script_pubkey == fallback).unwrap();
	assert_eq!(output.value, sat(98_000));
	assert!(!parent.output.iter().any(|output| output.script_pubkey == receive_destination));
	let mut value = sat(0);
	for input in &parent.input {
		let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
		value += previous.output[input.previous_output.vout as usize].value;
	}
	assert_eq!(parent.output.iter().map(|output| output.value).sum::<bitcoin::Amount>()
		+ super::expiry_test::actual_fee(&ctx, &parent).await, value);
	let height = ctx.generate_blocks(6).await;
	assert!(chain.get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
	assert!(chain.get_tx_out(&claim.compute_txid(), 0, Some(true)).unwrap().unwrap().confirmations >= 6);
	wait_partial_confirmation(&ctx,&srv,&db,height,sources[0].id(),claim.compute_txid(),unpaid,&parent).await;
}

/// The server records the confirmed source spend. The holder keeps the signed claim.
pub(super) async fn assert_retained_claim(db:&Db,source:VtxoId,claim:&Transaction) -> bool {
	db.read(async |t| {
		let row=t.query_opt("SELECT onchain_spent_txid,onchain_spent_height FROM vtxo WHERE vtxo_id=$1",
			&[&source.to_string()]).await?;
		Ok(row.is_some_and(|row| row.get::<_,Option<&str>>("onchain_spent_txid") == Some(claim.compute_txid().to_string().as_str())
			&& row.get::<_,Option<i32>>("onchain_spent_height").is_some()))
	}).await.unwrap()
}

pub(super) async fn wait_partial_confirmation(
	ctx:&TestContext,srv:&Captaind,db:&Db,height:BlockHeight,paid:VtxoId,claim:bitcoin::Txid,unpaid:VtxoId,parent:&Transaction,
) {
	srv.wait_for_sync_height(height).await;
	srv.watchmand().wait_for_sync_height(height).await;
	let expected_hash = ctx.bitcoind().sync_client().get_block_hash(height.to_u32() as u64).unwrap();
	let paid_source = paid.to_string();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let coherent = db.write(async |t| {
				let Some(view) = t.expiry_view().await? else { return Ok(false); };
				if view.height != height
					|| view.hash != expected_hash
					|| !(t.expiry_view().await? == Some(view))
				{ return Ok(false); }
				t.lock_watchman_tip(view).await?;
				let recorded = t.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c \
					JOIN expiry_entitlement e ON e.id=c.entitlement_id JOIN expiry_allocation_state a ON a.id=c.allocation_id \
					WHERE e.vtxo_id=$1 AND a.status<>'conflicted' AND a.status='confirmed' AND a.sweep_tx=$2",
					&[&unpaid.to_string(), &bitcoin::consensus::serialize(parent)]).await?.get::<_, i64>(0);
				if recorded != 1 { return Ok(false); }
				let source = t.query_one("SELECT v.onchain_spent_txid,h.offchain_resolution::TEXT,h.chain_resolution::TEXT \
					FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id WHERE v.vtxo_id=$1",&[&paid_source]).await?;
				assert_eq!(source.get::<_,Option<&str>>("onchain_spent_txid"),Some(claim.to_string().as_str()));
				assert_eq!(source.get::<_,Option<&str>>("chain_resolution"),Some("revoked"));
				assert!(source.get::<_,Option<&str>>("offchain_resolution").is_none(),
					"an actual user claim cannot be manufactured into permanent off-chain cancellation");
				let duplicate = t.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c \
					JOIN expiry_entitlement e ON e.id=c.entitlement_id JOIN expiry_allocation_state a ON a.id=c.allocation_id \
					WHERE e.vtxo_id=$1 AND c.disposition='customer' AND a.status='confirmed'", &[&paid_source]).await?
					.get::<_, i64>(0);
				assert_eq!(duplicate, 0, "the actual prior user receipt must not receive a second fallback");
				Ok(true)
			}).await.unwrap();
			if coherent { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("no-duplicate oracle requires an actually rebuilt canonical B payout");
}

#[tokio::test]
async fn expiry_internal_one_attempt_partial_terminal_keeps_other_source_owed() {
	Box::pin(partial_terminal()).await;
}
