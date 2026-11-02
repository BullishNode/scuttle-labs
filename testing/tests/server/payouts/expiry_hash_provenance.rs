use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::{PaymentHash, Preimage};
use ark::{ProtocolEncoding, VtxoPolicy};
use ark_testing::{btc, sat, TestContext};
use bark::vtxo::VtxoState;
use bark_json::exit::ExitState;
use bitcoin::{Amount, OutPoint};
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use server::database::expiry_ownership::OwnershipDisposition;
use server::database::Db;
use server_rpc::protos;

/// Two grants share a hash, but only one funds a paid attempt.
/// Its preimage must not collect the unused grant; ambiguous custody stays held.
#[tokio::test]
async fn expiry_unused_same_hash_grant_stays_held_after_other_source_paid() {
	let name = "server/expiry_unused_same_hash_grant_held";
	let ctx = TestContext::new(name).await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).no_vtxo_pool().funded(btc(10))
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); })
		.watchmand_cfg(|cfg| cfg.watchman.sweep_interval = Duration::from_secs(1)).create().await;
	let wallet = ctx.bark("owner", &srv).funded(sat(500_000)).create().await;
	let first = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await[0];
	let second = wallet.board_and_confirm_and_register(&ctx, sat(100_000)).await[0];
	let client = wallet.client().await;
	let preimage = Preimage::from([23; 32]);
	let hash = preimage.compute_payment_hash();
	let invoice = lightning.external.invoice_with_preimage(Some(sat(90_000)), "one-of-two-grants", "Exactly one source attempted", [23; 32]).await;
	let parsed: bark::lightning_invoice::Bolt11Invoice = invoice.parse().unwrap();
	assert_eq!(PaymentHash::from(&parsed), hash);
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let mut rpc = srv.get_public_rpc().await;
	let mut grants = Vec::new();
	let mut signed_grants = Vec::new();
	let mut sources = Vec::new();
	for id in [first, second] {
		let input = client.get_full_vtxo(id).await.unwrap();
		let key = client.get_vtxo_key(&input).await.unwrap();
		let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input.clone()],
			VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).unwrap()
			.generate_user_nonces(&[key]).unwrap();
		let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
			parts: super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
		}).await.unwrap().into_inner();
		let leaves = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
			.unwrap().build_signed_vtxos();
		client.register_vtxo_transactions_with_server(&leaves).await.unwrap();
		grants.push(leaves[0].id());
		signed_grants.push(leaves[0].clone());
		sources.push((input, key));
	}
	lightning.sync().await;
	rpc.initiate_lightning_payment(protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: 90_000, htlc_vtxo_ids: vec![grants[0].to_bytes().to_vec()],
		mailbox_id: None, retry_for_secs: Some(5),
	}).await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let paid_source = db.read(async |t| Ok(t.query_one("
				SELECT EXISTS(SELECT 1
					FROM lightning_payment_attempt a
					JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id = a.id
					WHERE l.vtxo_id = $1 AND a.payment_hash = $2 AND a.status = 'succeeded')
			",
				&[&grants[0].to_string(), &hash.to_string()]).await?.get::<_, bool>(0))).await.unwrap();
			if paid_source && db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap() == Some(preimage) {
				break;
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the actually submitted source's external invoice did not settle");
	for (index, id) in grants.iter().enumerate() {
		let count = db.read(async |t| Ok(t.query_one("
			SELECT COUNT(*)
			FROM lightning_payment_attempt_htlc_vtxo
			WHERE vtxo_id = $1
		", &[&id.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(count, if index == 0 { 1 } else { 0 }, "only the first signed grant funded the actual payment");
	}
	let height = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	srv.wait_for_sync_height(height).await;
	srv.watchmand().wait_for_sync_height(height).await;
	for (index, (input, _key)) in sources.iter().enumerate() {
		let funding = db.read(async |t|
			t.get_virtual_transaction_by_txid(input.chain_anchor().txid).await
		).await.unwrap().unwrap().signed_tx.unwrap().into_owned();
		let snapshot = db.read(async |t| t.backing_snapshot(&[ark::ServerVtxo::from(input.clone()).to_bare()]).await).await.unwrap();
		let fact = snapshot.regular.iter().find(|fact| fact.state.vtxo_id == grants[index]).unwrap();
		let proposal = db.read(async |t| t.propose_expiry_ownership(fact, height, Some(height)).await).await.unwrap();
		if index == 0 {
			assert_eq!(proposal.disposition, OwnershipDisposition::Operator, "the source that actually funded the paid invoice is discharged");
			assert_eq!(db.read(async |t| t.source_settlement(hash, grants[0]).await).await.unwrap(),
				server::database::ln::SourceSettlement::Funded);
			// Reconstruct only stale outcome metadata after genuine native
			// success. This is an integrity-divergence control, not a crash.
			db.write(async |t| {
				t.execute("UPDATE lightning_payment_attempt SET status='failed',updated_at=NOW() WHERE payment_hash=$1",
					&[&hash.to_string()]).await?;
				t.execute("UPDATE htlc_vtxo SET offchain_resolution=NULL WHERE id=(SELECT id FROM vtxo WHERE vtxo_id=$1)",
					&[&grants[0].to_string()]).await?;
				Ok(())
			}).await.unwrap();
			let held = db.read(async |t| t.propose_expiry_ownership(fact, height, Some(height)).await).await.unwrap();
			assert!(matches!(held.disposition, OwnershipDisposition::Unresolved(_)));
			assert!(held.settlement_integrity_hold, "cached Failed cannot authorize a paid-source refund or operator allocation");
			db.write(async |t| {
				t.execute("UPDATE lightning_payment_attempt SET status='succeeded',updated_at=NOW() WHERE payment_hash=$1",
					&[&hash.to_string()]).await?;
				t.execute("UPDATE htlc_vtxo SET offchain_resolution='fulfilled' WHERE id=(SELECT id FROM vtxo WHERE vtxo_id=$1)",
					&[&grants[0].to_string()]).await?;
				Ok(())
			}).await.unwrap();
		} else {
			assert_eq!(db.read(async |t| t.source_settlement(hash, grants[1]).await).await.unwrap(),
				server::database::ln::SourceSettlement::Unknown);
			assert!(matches!(proposal.disposition, OwnershipDisposition::Unresolved(_)),
				"the unused, unexpired source is not discharged by another source's preimage: {:?}", proposal.disposition);
		}
	}
	{
		assert_eq!(signed_grants[1].amount(), sat(100_000));
		let unused = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, grants[1]).await).await.unwrap().unwrap();
		assert_eq!(unused.htlc.offchain_resolution, None);
		assert_eq!(unused.vtxo.oor_spent_txid, None);
		let keys = sources.iter().map(|(_, key)| key.public_key()).collect::<Vec<_>>();
		for index in [0, 1] {
			let key = sources[index].1;
			let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([signed_grants[index].clone()],
				VtxoPolicy::new_pubkey(key.public_key())).unwrap().generate_user_nonces(&[key]).unwrap();
			let mut ids = grants.clone();
			ids.extend(sources.iter().map(|(input, _)| input.id()));
			ids.extend(builder.build_unsigned_vtxos().map(|v| v.id()));
			ids.extend(builder.build_unsigned_internal_vtxos().into_iter().map(|(v, _)| v.id()));
			let txids = builder.virtual_transactions().map(|id| id.to_string()).collect::<Vec<_>>();
			let source_before = super::expiry_admission::source_rows(&db, &ids).await;
			let owners_before = super::expiry_admission::owner_rows(&db, &keys).await;
			let ids_text = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
			let committed = async || db.read(async |t| Ok(t.query_one("SELECT jsonb_build_object(
				'htlcs',(SELECT jsonb_agg(to_jsonb(h) ORDER BY h.id) FROM htlc_vtxo h WHERE h.payment_hash=$2),
				'attempts',(SELECT jsonb_agg(to_jsonb(a) ORDER BY a.id) FROM lightning_payment_attempt a WHERE a.payment_hash=$2),
				'links',(SELECT jsonb_agg(to_jsonb(l) ORDER BY l.vtxo_id) FROM lightning_payment_attempt_htlc_vtxo l JOIN lightning_payment_attempt a ON a.id=l.lightning_payment_attempt_id WHERE a.payment_hash=$2),
				'wal',(SELECT jsonb_agg(to_jsonb(s) ORDER BY s.id) FROM htlc_settlement s WHERE s.payment_hash=$2),
				'owners',(SELECT jsonb_agg(to_jsonb(c) ORDER BY c.vtxo_id) FROM coin_owner c WHERE c.vtxo_id=ANY($1)),
				'virtual',(SELECT jsonb_agg(to_jsonb(v) ORDER BY v.txid) FROM virtual_transaction v WHERE v.txid=ANY($3)),
				'entitlements',(SELECT jsonb_agg(to_jsonb(e) ORDER BY e.id) FROM expiry_entitlement e WHERE e.vtxo_id=ANY($1)),
				'allocations',(SELECT jsonb_agg(to_jsonb(c) ORDER BY c.allocation_id,c.entitlement_id) FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=ANY($1)))::TEXT",
				&[&ids_text, &hash.to_string(), &txids]).await?.get::<_, String>(0))).await.unwrap();
			let before = committed().await;
			let response = rpc.request_lightning_pay_htlc_revocation(
				super::signed_package(&srv, &[&client], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await,
			).await;
			let error = response.expect_err("a settled hash must not be cooperatively refunded through either source");
			assert_eq!(error.code(), tonic::Code::InvalidArgument);
			assert!(error.message().ends_with(if index == 0 { "invoice has already been paid" }
				else { "source settlement provenance is unknown" }), "{error}");
			assert_eq!(committed().await, before);
			assert_eq!(super::expiry_admission::source_rows(&db, &ids).await, source_before);
			assert_eq!(super::expiry_admission::owner_rows(&db, &keys).await, owners_before);
		}
	}

	// Decision13 intentionally withdraws the former 99k automatic disjoint
	// refund. Exercise the real completed maturity view, not a fabricated row.
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(deadline.to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	let snapshot = db.read(async |t| t.backing_snapshot(&[ark::ServerVtxo::from(sources[1].0.clone()).to_bare()]).await).await.unwrap();
	let fact = snapshot.regular.iter().find(|fact| fact.state.vtxo_id == grants[1]).unwrap();
	let held = db.read(async |t| t.propose_expiry_ownership(fact, expired, Some(expired)).await).await.unwrap();
	assert!(matches!(held.disposition, OwnershipDisposition::Unresolved(_)));
	assert!(held.settlement_integrity_hold);
	let htlc = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, grants[1]).await).await.unwrap().unwrap();
	assert_eq!(htlc.htlc.resolution(), None);
	assert!(htlc.vtxo.oor_spent_txid.is_none());
	assert_eq!(htlc.amount(), sat(100_000));
	assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c
		JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1",
		&[&grants[1].to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);

	// Recover the registered body and reconcile its two locally stale parents.
	// Unknown still refuses cooperative/automatic refund; this uses B's user clause.
	assert!(client.get_vtxo_by_id(grants[1]).await.is_err());
	let report = client.recover_vtxos([first, second, grants[1]], None).await.unwrap();
	assert_eq!(report.recovered().ids().collect::<BTreeSet<_>>(), BTreeSet::from([grants[1]]));
	assert_eq!(report.skipped().ids().collect::<BTreeSet<_>>(), BTreeSet::from([first, second]));
	assert!(report.failed().is_empty() && report.foreign().is_empty()
		&& report.not_found().is_empty() && report.exited().is_empty());
	for id in [first, second] {
		assert_eq!(client.get_vtxo_by_id(id).await.unwrap().state, VtxoState::Spent);
	}
	assert!(client.get_vtxo_by_id(grants[0]).await.is_err());
	let recovered = client.get_full_vtxo(grants[1]).await.unwrap();
	assert_eq!(recovered.serialize(), signed_grants[1].serialize());
	assert!(recovered.has_all_witnesses());
	recovered.validate(&ctx.bitcoind().await_transaction(recovered.chain_anchor().txid).await).unwrap();
	assert_eq!(client.get_vtxo_key(&recovered).await.unwrap(), sources[1].1);
	let local = client.get_vtxo_by_id(grants[1]).await.unwrap();
	assert_eq!(local.state, VtxoState::Spendable);
	assert!(local.fallback_status.is_none());
	assert_eq!(db.read(async |t| t.expiry_owner(recovered.user_pubkey()).await).await.unwrap(),
		server::database::expiry_attribution::Attribution::Owned {
			mailbox_pk: client.mailbox_keypair().public_key(), spk: client.fallback_script(),
		});
	let fallback = client.fallback_script();
	drop(client);

	let mut native = lightning.internal.grpc_client().await;
	let paid = native.list_pays(cln_rpc::ListpaysRequest {
		payment_hash: Some(hash.to_vec()), ..Default::default()
	}).await.unwrap().into_inner().pays;
	assert_eq!(paid.len(), 1);
	assert_eq!(paid[0].status(), cln_rpc::listpays_pays::ListpaysPaysStatus::Complete);
	assert_eq!(paid[0].amount_msat.as_ref().unwrap().msat, 90_000_000);
	assert_eq!(paid[0].bolt11.as_ref().unwrap(), &parsed.to_string());
	let parts = native.list_send_pays(cln_rpc::ListsendpaysRequest {
		payment_hash: Some(hash.to_vec()), ..Default::default()
	}).await.unwrap().into_inner().payments;
	assert!(!parts.is_empty());
	assert!(parts.iter().all(|part| part.status() == cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Complete));
	assert_eq!(parts.iter().map(|part| part.amount_msat.as_ref().unwrap().msat).sum::<u64>(), 90_000_000);

	let core = ctx.bitcoind().sync_client();
	let scan_from = u64::from(tip.to_u32());
	let mut owner_before = BTreeMap::new();
	for coin in wallet.onchain_utxos().await {
		let tx = ctx.bitcoind().await_transaction(coin.outpoint.txid).await;
		let output = tx.output[coin.outpoint.vout as usize].clone();
		assert_eq!(output.value, coin.amount);
		assert!(owner_before.insert(coin.outpoint, output).is_none());
	}
	let balance_before = owner_before.values().map(|output| output.value).sum::<Amount>();
	assert_eq!(wallet.onchain_balance().await, balance_before);
	assert!(!owner_before.contains_key(&recovered.chain_anchor()));
	wallet.start_exit_vtxos([grants[1]]).await;
	ark_testing::exit::complete_exit(&ctx, &wallet).await;
	let exits = wallet.list_exits_with_txs().await;
	assert_eq!(exits.len(), 1);
	assert_eq!(exits[0].vtxo_id, grants[1]);
	assert!(matches!(exits[0].state, ExitState::Claimable(_)));
	let final_output = core.get_tx_out(&recovered.point().txid, recovered.point().vout, Some(true)).unwrap().unwrap();
	assert_eq!(final_output.value, sat(100_000));
	assert_eq!(final_output.script_pub_key.script().unwrap(), recovered.output_script_pubkey());
	assert!(final_output.confirmations >= u64::from((recovered.exit_delta() * 2).to_u32()));
	assert!(core.get_block_count().unwrap() >= u64::from(deadline.to_u32()));
	let address = wallet.get_onchain_address().await;
	assert_ne!(address.script_pubkey(), fallback);
	wallet.claim_exits([grants[1]], &address).await;
	let confirmed = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(confirmed).await;
	srv.watchmand().wait_for_sync_height(confirmed).await;
	wallet.progress_exit().await;
	let completed = wallet.list_exits_including_finished().await;
	assert_eq!(completed.len(), 1);
	assert_eq!(completed[0].vtxo_id, grants[1]);
	let ExitState::Claimed(claimed) = &completed[0].state else { panic!("B's own claim must complete"); };
	let claim = ctx.bitcoind().await_transaction(claimed.txid).await;
	assert_eq!(claim.input.len(), 1);
	assert_eq!(claim.input[0].previous_output, recovered.point());
	let script = recovered.policy().as_server_htlc_send().unwrap()
		.user_claim_after_expiry_clause(recovered.exit_delta()).tapscript();
	assert_eq!(claim.input[0].witness.taproot_leaf_script().unwrap().script, script.as_script());
	assert_eq!(claim.output.len(), 1);
	assert_eq!(claim.output[0].script_pubkey, address.script_pubkey());
	let claim_fee = super::expiry_test::actual_fee(&ctx, &claim).await;
	assert!(claim_fee > Amount::ZERO);
	assert_eq!(claim.output[0].value + claim_fee, sat(100_000));

	// Close the real parent/CPFP/claim graph over only B and existing owner coins.
	let mut graph = BTreeMap::new();
	for package in &exits[0].transactions {
		if let Some(previous) = graph.insert(package.exit.txid, package.exit.tx.clone()) {
			assert_eq!(previous, package.exit.tx);
		}
		if let Some(child) = &package.child {
			if let Some(previous) = graph.insert(child.info.txid, child.info.tx.clone()) {
				assert_eq!(previous, child.info.tx);
			}
		}
	}
	assert!(graph.insert(claimed.txid, claim.clone()).is_none());
	for step in recovered.transactions() {
		assert_eq!(graph.get(&step.tx.compute_txid()), Some(&step.tx));
	}
	let funding = ctx.bitcoind().await_transaction(recovered.chain_anchor().txid).await;
	let backing = funding.output[recovered.chain_anchor().vout as usize].clone();
	assert_eq!(backing.value, sat(100_000));
	let mut external = owner_before.clone();
	assert!(external.insert(recovered.chain_anchor(), backing).is_none());
	let mut spent = BTreeSet::new();
	let mut fees = Amount::ZERO;
	for (txid, tx) in &graph {
		assert_eq!(*txid, tx.compute_txid());
		super::expiry_test::canonical(&ctx, tx);
		fees += super::expiry_test::actual_fee(&ctx, tx).await;
		for input in &tx.input {
			assert!(spent.insert(input.previous_output));
			let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
			let expected = graph.get(&input.previous_output.txid)
				.map(|parent| &parent.output[input.previous_output.vout as usize])
				.or_else(|| external.get(&input.previous_output)).expect("only B and existing owner fee funds may enter");
			assert_eq!(&previous.output[input.previous_output.vout as usize], expected);
		}
	}
	assert!(spent.contains(&recovered.chain_anchor()) && spent.contains(&recovered.point()));
	let mut retained = owner_before.into_iter().filter(|(point, _)| !spent.contains(point)).collect::<BTreeMap<_, _>>();
	for (txid, tx) in &graph {
		for (index, output) in tx.output.iter().enumerate() {
			let point = OutPoint::new(*txid, index as u32);
			if output.value > Amount::ZERO && !spent.contains(&point) {
				assert!(retained.insert(point, output.clone()).is_none());
			}
		}
	}
	assert_eq!(retained.values().map(|output| output.value).sum::<Amount>() + fees, balance_before + sat(100_000));
	for _ in 0..2 {
		wallet.progress_exit().await;
		let mut owned = BTreeMap::new();
		for coin in wallet.onchain_utxos().await {
			let actual = core.get_tx_out(&coin.outpoint.txid, coin.outpoint.vout, Some(true)).unwrap().unwrap();
			assert!(actual.confirmations >= 6);
			owned.insert(coin.outpoint, bitcoin::TxOut { value: actual.value, script_pubkey: actual.script_pub_key.script().unwrap() });
		}
		assert_eq!(owned, retained, "all CPFP change and B's claim remain in the original owner's wallet");
		assert_eq!(wallet.onchain_balance().await + fees, balance_before + sat(100_000));
		assert_eq!(native.list_pays(cln_rpc::ListpaysRequest { payment_hash: Some(hash.to_vec()), ..Default::default() })
			.await.unwrap().into_inner().pays, paid);
		assert_eq!(native.list_send_pays(cln_rpc::ListsendpaysRequest { payment_hash: Some(hash.to_vec()), ..Default::default() })
			.await.unwrap().into_inner().payments, parts);
		assert_eq!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap(), Some(preimage));
		assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM cln_xpay WHERE payment_hash=$1",
			&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 1);
		assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM lightning_payment_attempt a
			JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id
			WHERE a.payment_hash=$1 AND a.status='succeeded' AND l.vtxo_id=$2", &[&hash.to_string(), &grants[0].to_string()])
			.await?.get::<_, i64>(0))).await.unwrap(), 1);
		assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM lightning_payment_attempt_htlc_vtxo WHERE vtxo_id=$1",
			&[&grants[1].to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
		assert_eq!(db.read(async |t| Ok(t.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c
			JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1", &[&grants[1].to_string()])
			.await?.get::<_, i64>(0))).await.unwrap(), 0);
		let observed = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, grants[1]).await).await.unwrap().unwrap();
		assert!(observed.htlc.offchain_resolution.is_none());
		assert_eq!(observed.htlc.chain_resolution, Some(server::database::htlc_vtxo::HtlcResolution::Revoked));
		assert_eq!(db.read(async |t| Ok(t.query_one("SELECT onchain_spent_txid FROM vtxo WHERE vtxo_id=$1",
			&[&grants[1].to_string()]).await?.get::<_, String>(0))).await.unwrap(), claimed.txid.to_string());
		let reader = wallet.client().await;
		for id in [first, second] { assert_eq!(reader.get_vtxo_by_id(id).await.unwrap().state, VtxoState::Spent); }
		assert_eq!(reader.get_vtxo_by_id(grants[1]).await.unwrap().state, VtxoState::Exited);
		let mut payments = Vec::new();
		for height in scan_from..=core.get_block_count().unwrap() {
			for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
				for output in &tx.output {
					assert_ne!(output.script_pubkey, fallback, "neither A nor B gets an automatic hash-only refund");
					if output.script_pubkey == address.script_pubkey() { payments.push((tx.compute_txid(), output.value)); }
				}
			}
		}
		assert_eq!(payments, vec![(claimed.txid, claim.output[0].value)]);
		for txid in core.get_raw_mempool().unwrap() {
			let tx = core.get_raw_transaction(&txid, None).unwrap();
			assert!(tx.input.iter().all(|input| input.previous_output != recovered.point()));
			assert!(tx.output.iter().all(|output| output.script_pubkey != address.script_pubkey() && output.script_pubkey != fallback));
		}
	}
}
