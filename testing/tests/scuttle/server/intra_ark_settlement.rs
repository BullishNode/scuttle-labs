use std::sync::Arc;
use std::time::Duration;

use ark::lightning::PaymentHash;
use ark::{ProtocolEncoding, Vtxo};
use ark::vtxo::Full;
use ark_testing::{btc, sat, TestContext};
use ark_testing::daemon::captaind::{ArkClient, proxy::ArkRpcProxy};
use bark::actions::lightning::receive::{LightningReceiveState, Progress as ReceiveProgress};
use bitcoin::Network;
use cln_rpc::plugins::hold;
use server::database::Db;
use server::database::ln::{LightningHtlcSubscriptionStatus, LightningPaymentStatus, SourceSettlement};
use server_rpc::protos;

async fn reopen(ctx: &TestContext, name: &str, mut config: bark::Config, url: String) -> bark::Wallet {
	let mnemonic = tokio::fs::read_to_string(ctx.datadir.join(name).join("mnemonic")).await.unwrap()
		.trim().parse::<bip39::Mnemonic>().unwrap();
	config.server_address = url;
	bark::Wallet::open(Network::Regtest, bark::WalletSeed::new_from_mnemonic(Network::Regtest, &mnemonic), config,
		bark::OpenWalletArgs {
			datadir: Some(ctx.datadir.join(name)), run_daemon: false, ..Default::default()
		},
	).await.unwrap()
}

async fn sender_snapshot(db: &Db, hash: PaymentHash, attempt: i64) -> serde_json::Value {
	db.read(async |tx| {
		let row = tx.query_one("SELECT status::TEXT, amount_msat, user_fee_sat,
			lightning_htlc_subscription_id, final_amount_msat,
			(SELECT COUNT(*) FROM lightning_payment_attempt_history WHERE id=a.id) AS history,
			(SELECT COUNT(*) FROM mailbox WHERE payment_hash=a.payment_hash
			 AND mailbox_type='ln-send-finished') AS notifications
			FROM lightning_payment_attempt a WHERE id=$1 AND payment_hash=$2",
			&[&attempt, &hash.to_string()]).await?;
		let sources = tx.query("SELECT v.vtxo_id,v.amount,v.spend_state::TEXT FROM vtxo v
			JOIN lightning_payment_attempt_htlc_vtxo l ON l.vtxo_id=v.vtxo_id
			WHERE l.lightning_payment_attempt_id=$1 ORDER BY v.vtxo_id", &[&attempt]).await?
			.into_iter().map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1), r.get::<_, String>(2)))
			.collect::<Vec<_>>();
		assert!(!sources.is_empty());
		assert!(sources.iter().all(|(_, _, state)| state == "ln-spent"));
		assert_eq!(sources.iter().map(|(_, amount, _)| amount).sum::<i64>(), 10_000);
		Ok(serde_json::json!({
			"status": row.get::<_, String>("status"), "amount_msat": row.get::<_, i64>("amount_msat"),
			"fee_sat": row.get::<_, Option<i64>>("user_fee_sat"),
			"subscription": row.get::<_, Option<i64>>("lightning_htlc_subscription_id"),
			"final_amount_msat": row.get::<_, Option<i64>>("final_amount_msat"),
			"history": row.get::<_, i64>("history"), "notifications": row.get::<_, i64>("notifications"),
			"sources": sources,
		}))
	}).await.unwrap()
}

/// Reconstruct the documented crash cut from a genuine completed intra-Ark
/// payment while the server is stopped. Only the receive status is rewound;
/// the original attempt, preimage, source spends and cancelled hold stay intact.
#[tokio::test]
async fn intra_ark_settlement_completes_after_crash_between_transactions() {
	let ctx = TestContext::new("scuttle/intra_ark_settlement_after_crash").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.htlc_settlement_poll_interval = Duration::from_millis(50)).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let payer = ctx.bark_sdk("payer", &srv).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let receiver = ctx.bark_sdk("receiver", &srv).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	payer.stop_daemon_wait().await.unwrap();
	receiver.stop_daemon_wait().await.unwrap();
	let invoice = receiver.bolt11_invoice(sat(10_000), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	Box::pin(payer.pay_lightning_invoice(invoice, None, false)).await.unwrap();
	srv.wait_for_lightning_receive_accepted(hash).await;
	assert!(matches!(Box::pin(receiver.try_claim_lightning_receive(hash, true)).await.unwrap(), LightningReceiveState::Settled(_)));
	Box::pin(payer.check_lightning_payment(hash, true)).await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let (subscription, attempt, preimage) = tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let sub = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap();
			let attempt = db.read(async |tx| tx.get_latest_payment_attempt_by_payment_hash(hash).await).await.unwrap().unwrap();
			if sub.status == LightningHtlcSubscriptionStatus::Settled && attempt.status == LightningPaymentStatus::Succeeded {
				assert_eq!(attempt.lightning_htlc_subscription_id, Some(sub.id));
				let preimage = db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().unwrap();
				break (sub.id, attempt.id, preimage);
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("initial genuine intra-Ark payment must settle");
	Box::pin(payer.sync()).await;
	Box::pin(receiver.sync()).await;
	assert_eq!(payer.balance().await.unwrap().spendable, sat(90_000));
	assert_eq!(receiver.balance().await.unwrap().spendable, sat(10_000));
	let payer_history = payer.history().await.unwrap();
	let receiver_history = receiver.history().await.unwrap();
	let before = sender_snapshot(&db, hash, attempt).await;
	assert_eq!(before["status"], "succeeded");
	assert_eq!(before["amount_msat"], 10_000_000);
	assert_eq!(before["fee_sat"], 0);
	assert_eq!(before["notifications"], 1);
	let mut hold_client = lightning.internal.hold_client().await;
	let native = hold_client.list(hold::ListRequest {
		constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner().invoices;
	assert_eq!(native.len(), 1);
	assert_eq!(native[0].state(), hold::InvoiceState::Cancelled);

	let payer_config = payer.config().clone();
	let receiver_config = receiver.config().clone();
	drop(payer);
	drop(receiver);
	srv.stop().await.unwrap();
	db.write(async |tx| {
		assert_eq!(tx.execute("UPDATE lightning_htlc_subscription SET status='htlcs-ready',updated_at=NOW()
			WHERE id=$1 AND status='settled'", &[&subscription]).await?, 1);
		Ok(())
	}).await.unwrap();
	assert!(db.read(async |tx| tx.get_open_lightning_payment_attempt_by_subscription_id(subscription).await).await.unwrap().is_none());
	assert_eq!(db.read(async |tx| tx.get_lightning_payment_attempt_by_subscription_id(subscription).await).await.unwrap().unwrap().id, attempt);
	assert_eq!(sender_snapshot(&db, hash, attempt).await, before);
	srv.start().await.unwrap();
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let status = db.read(async |tx| tx.get_htlc_subscription_by_id(subscription).await).await.unwrap().unwrap().status;
			if status == LightningHtlcSubscriptionStatus::Settled { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the original settlement must finish without calling the cancelled CLN hold");
	assert_eq!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap(), Some(preimage));
	assert_eq!(sender_snapshot(&db, hash, attempt).await, before, "no second attempt update, fee or send-finished notification");
	// The existing daemon allocates fresh RPC ports on each start. Reopen the
	// same wallets with only that endpoint updated, retaining keys and stores.
	let payer = Box::pin(reopen(&ctx, "payer", payer_config, srv.ark_url())).await;
	let receiver = Box::pin(reopen(&ctx, "receiver", receiver_config, srv.ark_url())).await;
	Box::pin(payer.sync()).await;
	Box::pin(receiver.sync()).await;
	assert_eq!(payer.balance().await.unwrap().spendable, sat(90_000));
	assert_eq!(receiver.balance().await.unwrap().spendable, sat(10_000));
	assert_eq!(payer.history().await.unwrap(), payer_history);
	assert_eq!(receiver.history().await.unwrap(), receiver_history);
	assert_eq!(sender_snapshot(&db, hash, attempt).await, before);
	let native = hold_client.list(hold::ListRequest {
		constraint: Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner().invoices;
	assert_eq!(native.len(), 1);
	assert_eq!(native[0].state(), hold::InvoiceState::Cancelled);
	assert!(lightning.internal.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
		payment_hash: Some(hash.to_vec()), ..Default::default()
	}).await.unwrap().into_inner().pays.is_empty(), "intra-Ark must not dispatch an external payment");
}


/// Interrupt the real two-transaction settlement before either the receive
/// status or cooperative grant spend commits. No financial row is reconstructed.
#[tokio::test]
async fn succeeded_receive_survives_crash_before_subscription_and_cosign_commit() {
	let ctx = TestContext::new("scuttle/succeeded_receive_before_subscription_and_cosign_commit").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| {
			cfg.htlc_settlement_poll_interval = Duration::from_millis(50);
			cfg.invoice_check_interval = Duration::from_secs(1);
		}).watchmand().create().await;
	srv.wait_for_vtxopool(&ctx).await;
	// It will later observe the real expired tip, then stop before recovery.
	srv.watchmand().stop().await.unwrap();
	let payer = ctx.bark_sdk("payer", &srv).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	#[derive(Clone)]
	struct HoldClaim {
		arrived: Arc<tokio::sync::Notify>,
		release: Arc<tokio::sync::Notify>,
	}
	#[async_trait::async_trait]
	impl ArkRpcProxy for HoldClaim {
		async fn claim_lightning_receive(
			&self, upstream: &mut ArkClient, req: protos::ClaimLightningReceiveRequest,
		) -> Result<protos::ArkoorPackageCosignResponse, tonic::Status> {
			self.arrived.notify_one();
			self.release.notified().await;
			Ok(upstream.claim_lightning_receive(req).await?.into_inner())
		}
	}
	let gate = HoldClaim { arrived: Arc::new(tokio::sync::Notify::new()), release: Arc::new(tokio::sync::Notify::new()) };
	let proxy = srv.start_proxy_no_mailbox(gate.clone()).await;
	let receiver = Arc::new(ctx.bark_sdk("receiver", &proxy).cfg(|cfg| cfg.daemon_manual_sync = true).create().await);
	payer.stop_daemon_wait().await.unwrap();
	receiver.stop_daemon_wait().await.unwrap();
	let invoice = receiver.bolt11_invoice(sat(10_000), None, None).await.unwrap();
	let hash = PaymentHash::from(&invoice);
	Box::pin(payer.pay_lightning_invoice(invoice, None, false)).await.unwrap();
	srv.wait_for_lightning_receive_accepted(hash).await;
	let claimant = receiver.clone();
	let claim = tokio::spawn(async move { Box::pin(claimant.try_claim_lightning_receive(hash, false)).await });
	tokio::time::timeout(Duration::from_secs(30), gate.arrived.notified()).await.unwrap();
	let checkpoint = receiver.lightning_receive_checkpoint(hash).await.unwrap().unwrap();
	let ReceiveProgress::PreimageRevealed(held) = &checkpoint.progress else {
		panic!("the real claim must have persisted its disclosure checkpoint");
	};
	assert_eq!(checkpoint.payment_hash, hash);
	assert!(checkpoint.claim_destination.is_none());
	let mut grants = Vec::new();
	for id in &held.vtxo_ids { grants.push(receiver.get_full_vtxo(*id).await.unwrap()); }
	assert!(!grants.is_empty());
	assert_eq!(grants.iter().map(Vtxo::amount).sum::<bitcoin::Amount>(), sat(10_000));
	let claim_key = grants[0].user_pubkey();
	assert!(grants.iter().all(|grant| grant.user_pubkey() == claim_key));
	assert_eq!(receiver.get_vtxo_key(&grants[0]).await.unwrap().public_key(), claim_key);
	let grant_ids = grants.iter().map(|grant| grant.id().to_string()).collect::<Vec<_>>();
	let expires = grants.iter().map(|grant| grant.policy().as_server_htlc_recv().unwrap().htlc_expiry).max().unwrap();
	let expiry_tip = expires + bitcoin_ext::BlockDelta::new(1);
	assert!(grants.iter().all(|grant| grant.expiry_height() > expiry_tip),
		"this cut needs the real HTLC expiry before the original coin expiry");
	let postgres = srv.config().postgres.clone();
	let db = Db::connect(&postgres).await.unwrap();
	let sub = db.read(async |tx| tx.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap();
	let attempt = db.read(async |tx| tx.get_lightning_payment_attempt_by_subscription_id(sub.id).await).await.unwrap().unwrap();
	assert_eq!(sub.status, LightningHtlcSubscriptionStatus::HtlcsReady);
	assert_eq!(attempt.status, LightningPaymentStatus::Requested);
	assert!(db.read(async |tx| tx.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	let native = lightning.internal.hold_client().await.list(hold::ListRequest {
		constraint:Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner().invoices;
	assert_eq!(native.len(),1); assert_eq!(native[0].state(),hold::InvoiceState::Cancelled);
	let receiver_mailbox = receiver.mailbox_keypair().public_key();
	// The first exact-P verification is in the WAL/prepared-destination Tx.
	// Every later verification is refused before the cosign Tx takes grant locks.
	db.write(async |tx| {
		tx.batch_execute(&format!("CREATE SEQUENCE issue62_claim_verifications;
			CREATE SEQUENCE issue62_settled_failures;
			CREATE FUNCTION issue62_refuse_cosign() RETURNS trigger AS $$
			BEGIN
			  IF NEW.user_pubkey='{claim_key}' THEN
			    IF nextval('issue62_claim_verifications') > 1 THEN
			      RAISE EXCEPTION 'ISSUE62 post-WAL cosign failure';
			    END IF;
			  END IF;
			  RETURN NEW;
			END; $$ LANGUAGE plpgsql;
			CREATE TRIGGER issue62_refuse_cosign BEFORE INSERT ON owner_key
			FOR EACH ROW EXECUTE FUNCTION issue62_refuse_cosign();
			CREATE FUNCTION issue62_refuse_settled() RETURNS trigger AS $$
			BEGIN
			  IF NEW.id={subscription} AND OLD.status='htlcs-ready' AND NEW.status='settled' THEN
			    PERFORM nextval('issue62_settled_failures');
			    RAISE EXCEPTION 'ISSUE62 subscription settled write failure';
			  END IF;
			  RETURN NEW;
			END; $$ LANGUAGE plpgsql;
			CREATE TRIGGER issue62_refuse_settled BEFORE UPDATE ON lightning_htlc_subscription
			FOR EACH ROW EXECUTE FUNCTION issue62_refuse_settled();", subscription=sub.id)).await?;
		Ok(())
	}).await.unwrap();
	gate.release.notify_one();
	assert!(tokio::time::timeout(Duration::from_secs(30), claim).await.unwrap().unwrap().is_err());
	assert!(receiver.lightning_receive_checkpoint(hash).await.unwrap().is_some_and(|cp|cp==checkpoint));
	let verification = db.read(async |tx| Ok(tx.query_one(
		"SELECT last_value FROM issue62_claim_verifications",&[]).await?.get::<_,i64>(0))).await.unwrap();
	assert_eq!(verification,2,"the genuine WAL verification passed, then only its cosign verification failed");
	// Both real failures must have executed, and the independent source-success
	// transaction must be visible before any crash is allowed.
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let ready = db.read(async |tx| {
				let a = tx.get_lightning_payment_attempt_by_subscription_id(sub.id).await?.unwrap();
				let failures = tx.query_one("SELECT last_value,is_called FROM issue62_settled_failures", &[]).await?;
				Ok(a.status == LightningPaymentStatus::Succeeded && failures.get::<_, bool>("is_called"))
			}).await.unwrap();
			if ready { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("the genuine source success must commit before the separate receive write fails");
	let cold_cut = async || {
		let state = db.read(async |tx| {
			let current = tx.get_htlc_subscription_by_id(sub.id).await?.unwrap();
			let wal = tx.get_htlc_settlement_by_payment_hash(hash).await?.unwrap();
			assert!(wal == checkpoint.payment_preimage, "the original genuine WAL must be retained");
			let rows = tx.query("SELECT c.source_vtxo_id,c.output_vtxo_id,c.output_vtxo,
				v.oor_spent_txid,h.offchain_resolution::TEXT,h.chain_resolution::TEXT,
				EXISTS(SELECT 1 FROM vtxo o WHERE o.vtxo_id=c.output_vtxo_id) AS output_inserted
				FROM expiry_lightning_receive_claim c JOIN vtxo v ON v.vtxo_id=c.source_vtxo_id
				JOIN htlc_vtxo h ON h.id=v.id WHERE c.subscription_id=$1 AND c.payment_hash=$2
				ORDER BY c.source_vtxo_id", &[&sub.id,&hash.to_string()]).await?;
			assert_eq!(rows.len(), grants.len());
			let mut prepared = Vec::new();
			for row in rows {
				let source: String = row.get("source_vtxo_id");
				let grant = grants.iter().find(|grant| grant.id().to_string() == source).unwrap();
				let output = Vtxo::<Full>::deserialize(row.get("output_vtxo")).unwrap();
				assert_eq!(output.amount(), grant.amount());
				assert_eq!(output.user_pubkey(), claim_key);
				assert!(row.get::<_, Option<String>>("oor_spent_txid").is_none());
				assert!(row.get::<_, Option<String>>("offchain_resolution").is_none());
				assert!(row.get::<_, Option<String>>("chain_resolution").is_none());
				assert!(!row.get::<_, bool>("output_inserted"));
				assert!(tx.get_virtual_transaction_by_txid(output.point().txid).await?.is_none());
				let owner = tx.query_one("SELECT mailbox_pk FROM owner_key WHERE user_pubkey=$1", &[&claim_key.to_string()]).await?;
				assert_eq!(owner.get::<_, String>(0), receiver_mailbox.to_string());
				prepared.push((source,row.get::<_, String>("output_vtxo_id"),output.amount().to_sat()));
			}
			let origins = tx.query("SELECT s.kind,s.attempt_id,s.subscription_id FROM htlc_settlement_source s
				JOIN htlc_settlement w ON w.id=s.settlement_id WHERE w.payment_hash=$1
				ORDER BY s.kind,s.attempt_id NULLS FIRST", &[&hash.to_string()]).await?
				.into_iter().map(|row|(row.get::<_, String>(0),row.get::<_, Option<i64>>(1),row.get::<_, Option<i64>>(2)))
				.collect::<Vec<_>>();
			assert_eq!(origins.len(),3);
			assert!(origins.contains(&("receive".into(),None,Some(sub.id))));
			assert!(origins.contains(&("receive".into(),Some(attempt.id),Some(sub.id))));
			assert!(origins.contains(&("attempt".into(),Some(attempt.id),None)));
			let ids = tx.query("SELECT vtxo_id FROM lightning_payment_attempt_htlc_vtxo
				WHERE lightning_payment_attempt_id=$1 ORDER BY vtxo_id", &[&attempt.id]).await?;
			for row in ids {
				let id = row.get::<_, String>(0).parse().unwrap();
				assert_eq!(tx.source_settlement(hash,id).await?,SourceSettlement::Funded);
			}
			Ok(serde_json::json!({"subscription":sub.id,"status":current.status.to_string(),
				"prepared":prepared,"origins":origins}))
		}).await.unwrap();
		let sender = sender_snapshot(&db,hash,attempt.id).await;
		serde_json::json!({"receive":state,"sender":sender})
	};
	let before = cold_cut().await;
	assert_eq!(before["receive"]["status"],"htlcs-ready");
	assert_eq!(before["sender"]["status"],"succeeded");
	assert_eq!(before["sender"]["amount_msat"],10_000_000);
	assert_eq!(before["sender"]["fee_sat"],0);
	assert_eq!(before["sender"]["notifications"],1);
	tokio::fs::write(ctx.datadir.join("issue62-before-crash.json"),serde_json::to_vec_pretty(&before).unwrap()).await.unwrap();
	let payer_config = payer.config().clone();
	let receiver_config = receiver.config().clone();
	drop(payer); drop(receiver); drop(proxy);
	let old_child = srv.native_child().await.unwrap();
	srv.crash_stop().await.unwrap();
	let after = cold_cut().await;
	tokio::fs::write(ctx.datadir.join("issue62-cold-crash.json"),serde_json::to_vec_pretty(&after).unwrap()).await.unwrap();
	assert_eq!(after,before,"the actual source success must survive the owned crash while both later writes rolled back");
	// Only the cosign fault is removed now. The real WAL worker will retry the
	// original subscription, but the Settled write remains faulty for the timer cut.
	db.write(async |tx| {
		tx.batch_execute("DROP TRIGGER issue62_refuse_cosign ON owner_key;
			DROP FUNCTION issue62_refuse_cosign(); DROP SEQUENCE issue62_claim_verifications;").await?;
		Ok(())
	}).await.unwrap();
	let current = ctx.bitcoind().get_block_count().await;
	assert!(current < u64::from(expiry_tip.to_u32()));
	ctx.generate_blocks(expiry_tip.to_u32()-u32::try_from(current).unwrap()).await;
	// The expiry predicate needs a REAL watchmand tip. Start it solely for
	// completed observation, then stop it before captain/timer/cooperative replay.
	srv.watchmand().start().await.unwrap();
	super::expiry_test::rebuilt(&ctx,&srv,&db).await;
	srv.watchmand().stop().await.unwrap();
	log::info!("ISSUE62 watchmand intentionally stopped at actual expired tip {}",expiry_tip);
	db.read(async |tx| {
		let view=tx.expiry_view().await?.unwrap();
		assert_eq!(view.processed.height,expiry_tip);
		assert!(tx.expiry_view_is_current(view).await?);
		assert_eq!(tx.query_one("SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=ANY($1)",
			&[&grant_ids]).await?.get::<_, i64>(0),0,"no fallback decision may replace this controlled cooperative tail");
		let same_hash=tx.query("SELECT v.vtxo_id FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id WHERE h.payment_hash=$1",
			&[&hash.to_string()]).await?.into_iter().map(|row|row.get::<_,String>(0)).collect::<Vec<_>>();
		assert_eq!(tx.query_one("SELECT COUNT(*) FROM vtxo WHERE vtxo_id=ANY($1) AND onchain_spent_txid IS NOT NULL",
			&[&same_hash]).await?.get::<_, i64>(0),0);
		assert_eq!(tx.query_one("SELECT COUNT(*) FROM expiry_allocation a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
			JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=ANY($1) AND a.broadcast_at IS NOT NULL",
			&[&same_hash]).await?.get::<_,i64>(0),0);
		Ok(())
	}).await.unwrap();
	srv.start().await.unwrap();
	srv.wait_for_sync_height(expiry_tip).await;
	assert_ne!(srv.native_child().await.unwrap(),old_child);
	let (guard_tx,mut guard_rx) = tokio::sync::mpsc::unbounded_channel();
	let expected_guard = format!("source settlement does not permit failure of payment attempt {}",attempt.id);
	let subscription_id=sub.id;
	let hash_string=hash.to_string();
	srv.add_stdout_handler(move |line:&str| {
		if let Ok(record) = server_log::parse_record(line) {
			if record.message == "Hold invoice timeout handling failed; will retry independently"
				&& record.extra.get("subscription_id").and_then(|v|v.as_i64()) == Some(subscription_id)
				&& record.extra.get("payment_hash").and_then(|v|v.as_str()) == Some(hash_string.as_str())
				&& record.extra.get("error").and_then(|v|v.as_str()).is_some_and(|error|error.contains(&expected_guard)) {
				let _ = guard_tx.send(()); return true;
			}
		}
		false
	});
	let refused = tokio::time::timeout(Duration::from_secs(30),guard_rx.recv()).await;
	let guarded = cold_cut().await;
	tokio::fs::write(ctx.datadir.join("issue62-expired-guard.json"),serde_json::to_vec_pretty(
		&serde_json::json!({"actual_guard_observed":refused.as_ref().is_ok_and(|event|event.is_some()),"cut":guarded})).unwrap()).await.unwrap();
	assert!(refused.unwrap().is_some(),"the actual expired hold timer must reach the source-failure guard");
	assert_eq!(guarded,before,"a failed cancellation must retain the original receive and paid source");
	assert_eq!(ctx.bitcoind().get_block_count().await,u64::from(expiry_tip.to_u32()));
	db.write(async |tx| {
		tx.batch_execute("DROP TRIGGER issue62_refuse_settled ON lightning_htlc_subscription;
			DROP FUNCTION issue62_refuse_settled(); DROP SEQUENCE issue62_settled_failures;").await?;
		Ok(())
	}).await.unwrap();
	tokio::time::timeout(Duration::from_secs(30),async {
		loop {
			let status = db.read(async |tx| tx.get_htlc_subscription_by_id(sub.id).await).await.unwrap().unwrap().status;
			if status == LightningHtlcSubscriptionStatus::Settled { break; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("normal WAL retry must settle the retained original receive");
	assert_eq!(sender_snapshot(&db,hash,attempt.id).await,before["sender"]);
	let payer = Box::pin(reopen(&ctx,"payer",payer_config.clone(),srv.ark_url())).await;
	let receiver = Box::pin(reopen(&ctx,"receiver",receiver_config.clone(),srv.ark_url())).await;
	assert!(receiver.lightning_receive_checkpoint(hash).await.unwrap().is_some_and(|cp|cp==checkpoint));
	assert!(matches!(Box::pin(receiver.try_claim_lightning_receive(hash,true)).await.unwrap(),LightningReceiveState::Settled(_)));
	Box::pin(payer.check_lightning_payment(hash,true)).await.unwrap();
	Box::pin(payer.sync()).await; Box::pin(receiver.sync()).await;
	assert_eq!(payer.balance().await.unwrap().spendable,sat(90_000));
	assert_eq!(receiver.balance().await.unwrap().spendable,sat(10_000));
	assert_eq!(payer.balance().await.unwrap().total()+receiver.balance().await.unwrap().total(),sat(100_000));
	let payer_history = payer.history().await.unwrap();
	let receiver_history = receiver.history().await.unwrap();
	let sends=payer_history.iter().filter(|m|m.subsystem.is_subsystem(bark::subsystem::Subsystem::LIGHTNING_SEND)).collect::<Vec<_>>();
	assert_eq!(sends.len(),1);
	assert_eq!(sends[0].status,bark::movement::MovementStatus::Successful);
	assert_eq!(sends[0].effective_balance,bitcoin::SignedAmount::from_sat(-10_000));
	assert_eq!(sends[0].offchain_fee,sat(0));
	assert_eq!(receiver_history.iter().filter(|m|m.subsystem.is_subsystem(bark::subsystem::Subsystem::LIGHTNING_RECEIVE)).count(),1);
	assert!(receiver.lightning_receive_checkpoint(hash).await.unwrap().is_none());
	assert!(receiver.vtxos().await.unwrap().iter().all(|coin|coin.fallback_status.is_none()),
		"a won fallback cannot be counted as this cooperative completion");
	drop(payer); drop(receiver);
	let payer = Box::pin(reopen(&ctx,"payer",payer_config,srv.ark_url())).await;
	let receiver = Box::pin(reopen(&ctx,"receiver",receiver_config,srv.ark_url())).await;
	Box::pin(payer.sync()).await; Box::pin(receiver.sync()).await;
	assert_eq!(payer.balance().await.unwrap().spendable,sat(90_000));
	assert_eq!(receiver.balance().await.unwrap().spendable,sat(10_000));
	assert!(payer.history().await.unwrap()==payer_history,"same payer history after reopen");
	assert!(receiver.history().await.unwrap()==receiver_history,"same receiver history after reopen");
	assert_eq!(sender_snapshot(&db,hash,attempt.id).await,before["sender"]);
	let native = lightning.internal.hold_client().await.list(hold::ListRequest {
		constraint:Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())),
	}).await.unwrap().into_inner().invoices;
	assert_eq!(native.len(),1); assert_eq!(native[0].state(),hold::InvoiceState::Cancelled);
	assert!(lightning.internal.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
		payment_hash:Some(hash.to_vec()),..Default::default()
	}).await.unwrap().into_inner().pays.is_empty());
}
