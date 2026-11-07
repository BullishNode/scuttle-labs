//! A send claim follows its own existing upstream payment attempt status.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ark::lightning::{Invoice, PaymentHash, Preimage};
use ark::vtxo::{Full, TapScriptClause};
use ark::{ServerVtxo, Vtxo, VtxoPolicy, SECP};
use ark_testing::{sat, Bark, TestContext};
use ark_testing::context::LightningPaymentSetup;
use ark_testing::daemon::captaind::Captaind;
use ark_testing::daemon::lightningd::Lightningd;
use bitcoin::{Amount, Transaction, TxIn, TxOut, Witness};
use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use bitcoin::taproot::{LeafVersion, TapLeafHash};
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use cln_rpc::listpays_pays::ListpaysPaysStatus;
use cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus;
use cln_rpc::plugins::hold;
use server::database::Db;
use server_rpc::protos;

use super::expiry_internal_partial_terminal::confirm_actual_path;
use super::expiry_send_claim::{assert_unspent, claim_server, confirmed_height, signed_leaf};

struct Dispatch {
	id: i64,
	label: String,
	amount_msat: u64,
}

struct ExternalPayment {
	lightning: LightningPaymentSetup,
	server: Arc<Captaind>,
	_sender: Bark,
	sources: Vec<Vtxo<Full>>,
	preimage: Preimage,
	db: Db,
	dispatch: Dispatch,
}

async fn signed_sources(
	ctx: &TestContext, server: &Captaind, sender: &Bark,
	hash: PaymentHash, amount: Amount, count: usize,
) -> Vec<Vtxo<Full>> {
	let mut boards = Vec::new();
	for _ in 0..count {
		let board = sender.board_and_confirm_and_register(ctx, amount).await;
		assert_eq!(board.len(), 1);
		boards.push(board[0]);
	}
	let wallet = sender.client().await;
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let expiry = tip + BlockDelta::new(286);
	let mut sources = Vec::new();
	for id in boards {
		let input = wallet.get_full_vtxo(id).await.unwrap();
		assert_eq!(input.amount(), amount);
		let key = wallet.get_vtxo_key(&input).await.unwrap();
		let source = signed_leaf(ctx, server, &wallet, input,
			VtxoPolicy::new_server_htlc_send(key.public_key(), hash, expiry)).await;
		assert_eq!(source.policy().as_server_htlc_send().unwrap().htlc_expiry, expiry);
		sources.push(source);
	}
	assert_eq!(sources.iter().map(Vtxo::chain_anchor).collect::<BTreeSet<_>>().len(), count);
	assert_eq!(sources.iter().map(Vtxo::id).collect::<BTreeSet<_>>().len(), count);
	sources
}

async fn external_dispatch(db: &Db, hash: PaymentHash, sources: &[Vtxo<Full>]) -> Dispatch {
	db.read(async |tx| {
		let rows = tx.query("SELECT a.id,a.amount_msat,a.user_fee_sat,a.max_routing_fee_msat, \
			a.lightning_htlc_subscription_id FROM lightning_payment_attempt a WHERE a.payment_hash=$1", &[&hash.to_string()]).await?;
		assert_eq!(rows.len(), 1, "one accepted payment must retain one upstream attempt");
		let id = rows[0].get::<_, i64>("id");
		let label = tx.query_one("SELECT label FROM cln_xpay WHERE payment_hash=$1", &[&hash.to_string()]).await?
			.get::<_, String>(0);
		assert!(!label.is_empty());
		let amount_msat = sources.iter().map(|source| source.amount().to_sat() * 1_000).sum::<u64>();
		assert_eq!(rows[0].get::<_, i64>("amount_msat"), i64::try_from(amount_msat).unwrap());
		assert_eq!(rows[0].get::<_, Option<i64>>("user_fee_sat"), Some(0));
		assert_eq!(rows[0].get::<_, Option<i64>>("max_routing_fee_msat"), Some(0));
		assert!(rows[0].get::<_, Option<i64>>("lightning_htlc_subscription_id").is_none());
		let links = tx.query("SELECT vtxo_id FROM lightning_payment_attempt_htlc_vtxo \
			WHERE lightning_payment_attempt_id=$1 ORDER BY vtxo_id", &[&id]).await?
			.into_iter().map(|row| row.get::<_, String>(0)).collect::<BTreeSet<_>>();
		assert_eq!(links, sources.iter().map(|source| source.id().to_string()).collect());
		assert_eq!(tx.query_one("SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1",
			&[&hash.to_string()]).await?.get::<_, i64>(0), 1);
		Ok(Dispatch { id, label, amount_msat })
	}).await.unwrap()
}

async fn native_parts(node: &Lightningd, hash: PaymentHash) -> Vec<cln_rpc::ListsendpaysPayments> {
	node.grpc_client().await.list_send_pays(cln_rpc::ListsendpaysRequest {
		payment_hash: Some(hash.to_vec()), ..Default::default()
	}).await.unwrap().into_inner().payments
}

async fn assert_native_pending(node: &Lightningd, hash: PaymentHash, dispatch: &Dispatch, tip: u32) -> u32 {
	let parts = native_parts(node, hash).await;
	assert!(!parts.is_empty());
	assert!(parts.iter().all(|part| part.payment_hash == hash.to_vec()
		&& part.label.as_deref() == Some(dispatch.label.as_str())
		&& part.status() == ListsendpaysPaymentsStatus::Pending && part.payment_preimage.is_none()));
	assert_eq!(parts.iter().map(|part| (part.groupid, part.partid)).collect::<BTreeSet<_>>().len(), parts.len());
	assert_eq!(parts.iter().map(|part| part.amount_msat.as_ref().unwrap().msat).sum::<u64>(), dispatch.amount_msat);
	let htlcs = node.grpc_client().await.list_htlcs(cln_rpc::ListhtlcsRequest {
		id: None, index: None, start: None, limit: None,
	}).await.unwrap().into_inner().htlcs.into_iter()
		.filter(|htlc| htlc.payment_hash == hash.to_vec()).collect::<Vec<_>>();
	assert!(!htlcs.is_empty(), "accepted payment must have durable native channel HTLCs");
	assert!(htlcs.iter().all(|htlc| htlc.direction() == cln_rpc::listhtlcs_htlcs::ListhtlcsHtlcsDirection::Out
		&& htlc.state() == cln_rpc::HtlcState::SentAddAckRevocation && htlc.expiry > tip));
	assert_eq!(htlcs.iter().map(|htlc| htlc.amount_msat.as_ref().unwrap().msat).sum::<u64>(), dispatch.amount_msat);
	htlcs.iter().map(|htlc| htlc.expiry).min().unwrap()
}

async fn wait_native_complete(
	node: &Lightningd, hash: PaymentHash, preimage: Preimage, dispatch: &Dispatch,
) -> cln_rpc::ListpaysPays {
	tokio::time::timeout(Duration::from_secs(20), async {
		loop {
			let pays = node.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
				bolt11: None, payment_hash: Some(hash.to_vec()), status: None,
				index: None, limit: None, start: None,
			}).await.unwrap().into_inner().pays;
			assert_eq!(pays.len(), 1, "reconciliation must not initiate another native payment");
			if pays[0].status() != ListpaysPaysStatus::Complete {
				tokio::time::sleep(Duration::from_millis(20)).await;
				continue;
			}
			let pay = &pays[0];
			assert_eq!(pay.payment_hash, hash.to_vec());
			assert_eq!(pay.label.as_deref(), Some(dispatch.label.as_str()));
			assert_eq!(pay.preimage.as_deref(), Some(preimage.as_ref()));
			assert_eq!(pay.amount_msat.as_ref().unwrap().msat, dispatch.amount_msat);
			assert_eq!(pay.amount_sent_msat.as_ref().unwrap().msat, dispatch.amount_msat,
				"the direct zero-fee channel must preserve the declared principal");
			let parts = native_parts(node, hash).await;
			let created = pay.created_index.filter(|index| *index > 0).unwrap();
			let first = parts.iter().find(|part| part.created_index == created).unwrap();
			assert!(parts.iter().all(|part| part.groupid == first.groupid
				&& part.label.as_deref() == Some(dispatch.label.as_str()) && part.payment_hash == hash.to_vec()));
			assert_eq!(parts.iter().map(|part| part.created_index).min().unwrap(), created);
			assert_eq!(parts.iter().map(|part| (part.groupid, part.partid)).collect::<BTreeSet<_>>().len(), parts.len());
			let mut completed = 0;
			for part in &parts {
				match part.status() {
					ListsendpaysPaymentsStatus::Complete => {
						assert_eq!(part.payment_preimage.as_deref(), Some(preimage.as_ref()));
						completed += part.amount_msat.as_ref().unwrap().msat;
					},
					ListsendpaysPaymentsStatus::Failed => assert!(part.payment_preimage.is_none()),
					_ => panic!("a completed native group still has an undecided part"),
				}
			}
			assert_eq!(completed, dispatch.amount_msat);
			return pays.into_iter().next().unwrap();
		}
	}).await.expect("receiver settlement must complete its own native payment")
}

async fn wait_upstream_success(db: &Db, hash: PaymentHash, dispatch: &Dispatch, native: &cln_rpc::ListpaysPays) {
	tokio::time::timeout(Duration::from_secs(20), async {
		loop {
			let succeeded = db.read(async |tx| Ok(tx.query_one("SELECT status='succeeded',final_amount_msat \
				FROM lightning_payment_attempt WHERE id=$1 AND payment_hash=$2", &[&dispatch.id, &hash.to_string()]).await?)).await.unwrap();
			if succeeded.get::<_, bool>(0) {
				assert_eq!(succeeded.get::<_, Option<i64>>(1), Some(i64::try_from(native.amount_sent_msat.as_ref().unwrap().msat).unwrap()));
				assert_eq!(native.amount_msat.as_ref().unwrap().msat, dispatch.amount_msat);
				assert!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_some());
				break;
			}
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	}).await.expect("the native outcome must update its existing upstream attempt");
}

async fn external_payment(ctx: &TestContext, count: usize, selected: usize) -> ExternalPayment {
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let server = claim_server(ctx, &lightning.internal).await;
	let sender = ctx.bark("sender", &server).cfg(|cfg| cfg.daemon_manual_sync = true)
		.funded(sat(500_000)).create().await;
	let preimage = Preimage::random();
	let hash = preimage.compute_payment_hash();
	// Two 90k sources fund precisely 180k. There is no hidden 20k surplus
	// or synthetic routing fee in the direct-channel accounting.
	let sources = signed_sources(ctx, &server, &sender, hash, sat(90_000), count).await;
	let sender_client = sender.client().await;
	let mut source_keys = Vec::new();
	for source in &sources[..selected] {
		source_keys.push(sender_client.pubkey_keypair(&source.user_pubkey())
			.await.unwrap().unwrap().1);
	}
	let amount = sources[..selected].iter().map(Vtxo::amount).sum::<Amount>();
	let mut receiver = lightning.external.hold_client().await;
	let invoice = receiver.invoice(hold::InvoiceRequest {
		payment_hash: hash.to_vec(), amount_msat: amount.to_sat() * 1_000,
		description: Some(hold::invoice_request::Description::Memo("send claim status".into())),
		min_final_cltv_expiry: Some(220), expiry: Some(3600), routing_hints: vec![],
	}).await.unwrap().into_inner().bolt11;
	lightning.sync().await;
	let admission_tip = ctx.bitcoind().get_block_count().await as u32;
	let request = protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: amount.to_sat(),
		htlc_vtxo_ids: sources[..selected].iter().map(|source| source.id().to_bytes().to_vec()).collect(),
		mailbox_id: None, retry_for_secs: Some(1),
		intent_nonce: None, source_owner_signatures: Vec::new(),
	};
	let signed = ark_testing::lightning::sign_lightning_send_request_with_keys(
		&server, request.clone(), &source_keys,
	).await;
	server.get_public_rpc().await.initiate_lightning_payment(signed).await.unwrap();
	server.get_public_rpc().await.initiate_lightning_payment(request).await
		.expect("an old client may acknowledge its already open payment");
	lightning.external.wait_for_hold_invoice_accepted(hash).await;
	let db = Db::connect(&server.config().postgres).await.unwrap();
	let dispatch = external_dispatch(&db, hash, &sources[..selected]).await;
	let native_expiry = assert_native_pending(&lightning.internal, hash, &dispatch,
		ctx.bitcoind().get_block_count().await as u32).await;
	assert!(native_expiry >= admission_tip + 220);
	assert!(sources[..selected].iter().all(|source| native_expiry < source.policy().as_server_htlc_send().unwrap().htlc_expiry.to_u32()));
	assert!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	ExternalPayment { lightning, server, _sender: sender, sources, preimage, db, dispatch }
}

async fn expose_and_mature(ctx: &TestContext, server: &Captaind, sources: &[Vtxo<Full>]) {
	let mut seen = BTreeSet::new();
	for source in sources { confirm_actual_path(ctx, source, &mut seen).await; }
	let mature = sources.iter().map(|source| {
		let policy = source.policy().as_server_htlc_send().unwrap();
		let clause = policy.server_reveals_preimage_clause(source.server_pubkey(), source.exit_delta());
		assert_eq!(clause.block_delta, BlockDelta::new(144));
		confirmed_height(ctx, source) + clause.block_delta.to_u32() + 1
	}).max().unwrap();
	let tip = ctx.bitcoind().get_block_count().await as u32;
	if mature > tip { ctx.generate_blocks(mature - tip).await; }
	let height = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	server.wait_for_sync_height(height).await;
	server.watchmand().wait_for_sync_height(height).await;
	for source in sources {
		let confirmed = confirmed_height(ctx, source);
		assert!(height.to_u32() > confirmed + 144);
		assert!(height < source.policy().as_server_htlc_send().unwrap().htlc_expiry);
		assert!(height.to_u32() < confirmed + 288);
		assert!(height < source.expiry_height());
		assert_unspent(ctx, source);
	}
}

#[tokio::test]
async fn completed_hash_cannot_discharge_another_queued_source() {
	let ctx = TestContext::new("server/completed_hash_cannot_discharge_another_queued_source").await;
	tokio::time::timeout(Duration::from_secs(180), async {
		let payment = external_payment(&ctx, 2, 1).await;
		let hash = payment.preimage.compute_payment_hash();
		payment.lightning.external.hold_client().await.settle(hold::SettleRequest {
			payment_preimage: payment.preimage.to_vec(),
		}).await.unwrap();
		let native = wait_native_complete(&payment.lightning.internal, hash, payment.preimage, &payment.dispatch).await;
		wait_upstream_success(&payment.db, hash, &payment.dispatch, &native).await;
		let native_before = native_parts(&payment.lightning.internal, hash).await;
		let omitted = &payment.sources[1];
		let invoice = Invoice::from_str(native.bolt11.as_deref().unwrap()).unwrap();
		let queued = payment.db.write(async |tx| {
			let first = tx.get_latest_payment_attempt_by_payment_hash(hash).await?.unwrap();
			assert_eq!(first.status, server::database::ln::LightningPaymentStatus::Succeeded);
			tx.store_lightning_payment_start(first.lightning_node_id, &invoice, omitted.amount(), None,
				&[omitted.id()], None, BlockHeight::new(ctx.bitcoind().get_block_count().await as u32),
				Amount::ZERO, None, Duration::from_secs(1), Some(Amount::ZERO), None).await?;
			Ok(tx.get_open_lightning_payment_attempt_by_payment_hash(hash).await?.unwrap().id)
		}).await.unwrap();
		assert_ne!(queued, payment.dispatch.id);
		// B has no native dispatch of its own. Empty own history keeps it
		// held; A's completed hash cannot authorize B's mature server claim.
		assert_eq!(native_parts(&payment.lightning.internal, hash).await, native_before);
		expose_and_mature(&ctx, &payment.server, &[omitted.clone()]).await;
		let error = payment.db.write(async |tx| {
			let refusal = tx.lock_watchman_claim_inputs(&[ServerVtxo::from(omitted.to_bare())]).await;
			let held = tx.get_open_lightning_payment_attempt_by_payment_hash(hash).await?
				.expect("the unattempted queued source must remain held");
			assert_eq!(held.id, queued);
			assert_eq!(held.status, server::database::ln::LightningPaymentStatus::Requested);
			refusal
		}).await.expect_err("the server cannot claim the unpaid queued source");
		assert!(format!("{error:#}").contains("Lightning source is not discharged"));
		assert_unspent(&ctx, omitted);
		let policy = omitted.policy().as_server_htlc_send().unwrap();
		let clause = policy.user_claim_after_expiry_clause(omitted.exit_delta());
		let mature = std::cmp::max(policy.htlc_expiry.to_u32(), confirmed_height(&ctx, omitted) + clause.block_delta.to_u32()) + 1;
		let tip = ctx.bitcoind().get_block_count().await as u32;
		if mature > tip { ctx.generate_blocks(mature - tip).await; }
		let destination = ctx.bitcoind().get_new_address();
		let fee = sat(2_000);
		let mut refund = Transaction {
			version: bitcoin::transaction::Version::TWO, lock_time: clause.locktime(),
			input: vec![TxIn { previous_output: omitted.point(), script_sig: Default::default(),
				sequence: clause.sequence(), witness: Witness::new() }],
			output: vec![TxOut { value: omitted.amount() - fee, script_pubkey: destination.script_pubkey() }],
		};
		let control = clause.control_block(omitted);
		let sighash = SighashCache::new(&refund).taproot_script_spend_signature_hash(0,
			&Prevouts::All(&[omitted.txout()]), TapLeafHash::from_script(&clause.tapscript(), LeafVersion::TapScript),
			TapSighashType::Default).unwrap();
		let key = payment._sender.client().await.get_vtxo_key(omitted).await.unwrap();
		let signature = SECP.sign_schnorr(&sighash.into(), &key);
		refund.input[0].witness = clause.witness(&signature, &control);
		let chain = ctx.bitcoind().sync_client();
		assert!(chain.test_mempool_accept(&[&refund]).unwrap()[0].allowed);
		chain.send_raw_transaction(&refund).unwrap();
		ctx.generate_blocks(6).await;
		assert!(chain.get_tx_out(&omitted.point().txid, omitted.point().vout, Some(false)).unwrap().is_none());
		let paid = chain.get_tx_out(&refund.compute_txid(), 0, Some(false)).unwrap().unwrap();
		assert!(paid.confirmations >= 6);
		assert_eq!(paid.value + fee, omitted.amount());
		assert_eq!(paid.script_pub_key.script().unwrap(), destination.script_pubkey());
		assert_eq!(native_parts(&payment.lightning.internal, hash).await, native_before,
			"returning the unpaid source cannot pay the native invoice a second time");
	}).await.expect("queued-source identity and refund exceeded 180 seconds");
}
