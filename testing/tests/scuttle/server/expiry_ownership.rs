use ark::arkoor::package::ArkoorPackageBuilder;
use ark::lightning::PaymentHash;
use ark::{ProtocolEncoding, VtxoPolicy};
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use server_rpc::protos;
use server_log::ExpiryHoldObserved;

use ark_testing::{btc, sat, TestContext};
use std::time::Duration;

#[tokio::test]
async fn held_coin_is_alarmed_before_decision() {
	abandoned_registered_send(true, false).await;
}

async fn abandoned_registered_send(check_alarms: bool, lagged_captain: bool) {
	let name = if lagged_captain { "server/expiry_completed_watchman_lagged_captain" }
		else if check_alarms { "server/held_coin_is_alarmed_before_decision" }
		else { "server/expiry_abandoned_registered_send" };
	let ctx = TestContext::new(name).await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let builder = ctx.captaind("server").lightningd(&lightning.internal)
		.funded(btc(10)).cfg(|cfg| cfg.vtxo_lifetime = BlockDelta::new(24));
	let srv = if lagged_captain {
		let srv = builder.create().await;
		srv.attach_watchmand(ctx.watchmand("server-watchmand").bitcoind(ctx.bitcoind_arc())
			.cfg(|cfg| cfg.watchman.sweep_interval = std::time::Duration::from_secs(3600))
			.create(&srv).await);
		srv
	} else {
		builder.watchmand_cfg(|cfg| cfg.watchman.sweep_interval = std::time::Duration::from_secs(3600))
			.create().await
	};
	let mut alarms = check_alarms.then(|| srv.watchmand().subscribe_log::<ExpiryHoldObserved>());
	let sender = ctx.bark("sender", &srv).funded(sat(500_000)).create().await;
	let ids = sender.board_and_confirm_and_register(&ctx, sat(100_000)).await;
	let wallet = sender.client().await;
	let input = wallet.get_full_vtxo(ids[0]).await.unwrap();
	let key = wallet.get_vtxo_key(&input).await.unwrap();
	let preimage_bytes = [12; 32];
	let invoice = lightning.external.invoice_with_preimage(Some(sat(100_000)), "abandoned", "Never initiated", preimage_bytes).await;
	let parsed: bark::lightning_invoice::Bolt11Invoice = invoice.parse().unwrap();
	let hash = PaymentHash::from(&parsed);
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		[input.clone()], VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline),
	).unwrap().generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
	}).await.unwrap().into_inner();
	let response = ark::arkoor::package::ArkoorPackageCosignResponse::try_from(response).unwrap();
	let htlcs = builder.user_cosign(&[key], response).unwrap().build_signed_vtxos();
	wallet.register_vtxo_transactions_with_server(&htlcs).await.unwrap();
	let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
	let htlc = &htlcs[0];
	assert!(input.expiry_height() < deadline);
	let cut = async || {
		db.read(async |tx| {
			let id = htlc.id().to_string();
			let row = tx.query_one("SELECT \
				(SELECT COUNT(*) FROM expiry_entitlement WHERE vtxo_id=$1) AS entitlements, \
				(SELECT COUNT(*) FROM expiry_allocation_input WHERE vtxo_id=$1) AS reservations, \
				(SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$2) AS attempts, \
				(SELECT COUNT(*) FROM cln_xpay WHERE payment_hash=$2) AS requests, \
				(SELECT COUNT(*) FROM lightning_payment_attempt_htlc_vtxo WHERE vtxo_id=$1) AS links, \
				(SELECT COUNT(*) FROM htlc_settlement WHERE payment_hash=$2) AS wal, \
				(SELECT COUNT(*) FROM vtxo WHERE vtxo_id=$1 AND (confirmed_height IS NOT NULL OR onchain_spent_txid IS NOT NULL)) AS exits",
				&[&id,&hash.to_string()]).await?;
			Ok(["entitlements","reservations","attempts","requests","links","wal","exits"].into_iter()
				.map(|key| (key.to_owned(),row.get::<_, i64>(key))).collect::<std::collections::BTreeMap<_,_>>())
		}).await.unwrap()
	};
	let expired = if lagged_captain {
		let c = BlockHeight::new(srv.bitcoind().get_block_count().await as u32);
		srv.wait_for_sync_height(c).await;
		assert!(c < input.expiry_height());
		assert!(!srv.bitcoind().sync_client().call::<bool>("setnetworkactive", &[serde_json::json!(false)]).unwrap());
		let before = ctx.bitcoind().get_block_count().await as u32;
		assert!(before < input.expiry_height().to_u32());
		ctx.bitcoind().generate(input.expiry_height().to_u32() + 1 - before).await;
		BlockHeight::new(ctx.bitcoind().get_block_count().await as u32)
	} else {
		ctx.generate_blocks(input.expiry_height().to_u32() + 1 - tip.to_u32()).await
	};
	srv.watchmand().wait_for_sync_height(expired).await;
	let completed = if lagged_captain {
		let canonical = ctx.bitcoind().sync_client().get_block_hash(u64::from(expired.to_u32())).unwrap();
		let view = tokio::time::timeout(Duration::from_secs(15), async {
			loop {
				if let Some(view) = db.read(async |tx| {
					let Some(view) = tx.expiry_view().await? else { return Ok(None); };
					Ok(if (tx.expiry_view().await? == Some(view)) { Some(view) } else { None })
				}).await.unwrap() {
					if view.height==expired && view.hash==canonical { break view; }
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the advancing watchmand must publish its real valid completed primary view");
		let c = db.read(async |tx| tx.get_highest_block(server::database::BlockTable::Captaind).await)
			.await.unwrap().unwrap();
		assert!(c.height < input.expiry_height() && input.expiry_height() <= view.height && view.height < deadline);
		assert_eq!(srv.bitcoind().get_block_count().await, u64::from(c.height.to_u32()), "captain Core RPC must remain genuinely lagged/alive");
		lightning.internal.wait_for_block(expired).await;
		assert!(cut().await.values().all(|count| *count==0));
		let stored = db.read(async |tx| tx.get_user_vtxo_by_id(htlc.id()).await).await.unwrap();
		assert_eq!(stored.vtxo.serialize(), htlc.serialize());
		assert_eq!(stored.spend_state, server::database::SpendState::Spendable);
		assert!(stored.confirmed_height.is_none());
		assert!(!stored.decided_for_fallback);
		Some((c.height,view))
	} else { None };
	let response = rpc.initiate_lightning_payment(protos::InitiateLightningPaymentRequest {
		invoice, payment_amount_sat: 100_000,
		htlc_vtxo_ids: htlcs.iter().map(|v| v.id().to_bytes().to_vec()).collect(),
		mailbox_id: None, retry_for_secs: None,
	}).await;
	if let Some((captain,watchman)) = completed {
		let counts = cut().await;
		let pays = lightning.internal.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
			bolt11:None,payment_hash:Some(hash.to_vec()),status:None,index:None,limit:None,start:None,
		}).await.unwrap().into_inner().pays;
		let parts = lightning.internal.grpc_client().await.list_send_pays(cln_rpc::ListsendpaysRequest {
			payment_hash:Some(hash.to_vec()),..Default::default()
		}).await.unwrap().into_inner().payments;
		// Preserve an authentic old-source admission/counters before the expected
		// refusal assertion; never continue its unattempted-refund tail if admitted.
		tokio::fs::write(ctx.datadir.join("lagged-admission-observation.json"), serde_json::to_vec_pretty(&serde_json::json!({
			"captain_height":captain.to_u32(),"watchman_height":watchman.height.to_u32(),
			"watchman_hash":watchman.hash.to_string(),
			"source_expiry":input.expiry_height().to_u32(),"htlc_expiry":deadline.to_u32(),
			"source":htlc.id().to_string(),"rpc_ok":response.is_ok(),
			"rpc_error":response.as_ref().err().map(|error| serde_json::json!({"code":error.code() as i32,
				"message":error.message(),"reason":error.metadata().get("abandon-reason").and_then(|x|x.to_str().ok()),
				"identifiers":error.metadata().get("identifiers").and_then(|x|x.to_str().ok())})),
			"counts":counts,"native_pay_groups":pays.len(),"native_parts":parts.len(),
		})).unwrap()).await.unwrap();
		let error = response.as_ref().expect_err("completed watchmand maturity must refuse new work despite lagged captain");
		assert_eq!(error.code(),tonic::Code::InvalidArgument);
		assert_eq!(error.metadata().get("abandon-reason").unwrap().to_str().unwrap(),"expired_new_lightning");
		assert_eq!(error.metadata().get("identifiers").unwrap().to_str().unwrap(),htlc.id().to_string());
		assert!(counts.values().all(|count| *count==0));
		assert!(pays.is_empty() && parts.is_empty());
		let stored = db.read(async |tx| tx.get_user_vtxo_by_id(htlc.id()).await).await.unwrap();
		assert_eq!(stored.vtxo.serialize(),htlc.serialize());
		assert_eq!(stored.spend_state,server::database::SpendState::Spendable);
		assert!(stored.confirmed_height.is_none());
		assert!(srv.bitcoind().sync_client().call::<bool>("setnetworkactive", &[serde_json::json!(true)]).unwrap());
		ctx.await_block_count_sync().await;
		srv.wait_for_sync_height(expired).await;
	}
	let error = response.expect_err("offline sender cannot start an external payment after backing expiry");
	assert!(error.message().contains("expired"), "{error}");
	let attempts = db.read(async |t| t.get_latest_payment_attempt_by_payment_hash(hash).await).await.unwrap();
	assert!(attempts.is_none());
	let root = ark::ServerVtxo::from(input.clone()).to_bare();
	let funding = db.read(async |t| t.get_virtual_transaction_by_txid(input.chain_anchor().txid).await).await.unwrap().unwrap()
		.signed_tx.unwrap().into_owned();
	let snapshot = db.read(async |t| t.backing_snapshot(&[root.clone()]).await).await.unwrap();
	let fact = snapshot.regular.iter().find(|f| f.state.vtxo_id == htlc.id()).unwrap();
	let pending = db.read(async |t| t.propose_expiry_ownership(fact, expired, Some(expired)).await).await.unwrap();
	assert!(matches!(pending.disposition, server::database::expiry_ownership::OwnershipDisposition::Unresolved(_)));
	if check_alarms {
		tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				let alarm = alarms.as_mut().unwrap().recv().await.expect("expiry alarm stream closed");
				if alarm.vtxo_id == htlc.id() {
					assert_eq!(alarm.reason, "Lightning settlement pending");
					break;
				}
			}
		}).await.expect("the genuine unresolved source must be alarmed before its deadline");
	}
	let tip = ctx.generate_blocks(deadline.to_u32() + 1 - expired.to_u32()).await;
	srv.watchmand().wait_for_sync_height(tip).await;
	let snapshot = db.read(async |t| t.backing_snapshot(&[root.clone()]).await).await.unwrap();
	let fact = snapshot.regular.iter().find(|f| f.state.vtxo_id == htlc.id()).unwrap();
	let ready = db.read(async |t| t.propose_expiry_ownership(fact, tip, Some(tip)).await).await.unwrap();
	assert!(matches!(ready.disposition,
		server::database::expiry_ownership::OwnershipDisposition::Customer { user_pubkey, .. }
			if user_pubkey == key.public_key()
	));
	assert!(matches!(ready.evidence, Some(
		server::database::expiry_ownership::OwnershipEvidence::HtlcUnattemptedExpired { .. }
			| server::database::expiry_ownership::OwnershipEvidence::IrreversibleEntitlement { .. }
	)));
	// Readiness never invents an upstream off-chain revocation. A concurrent
	// background allocation may already have recorded the spend fence.
	let upstream = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, htlc.id()).await).await.unwrap().unwrap();
	assert_eq!(upstream.htlc.resolution(), None);
	if check_alarms {
		tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				let decided = db.read(async |tx| Ok(tx.query_one("SELECT EXISTS(SELECT 1 FROM expiry_entitlement e \
					JOIN expiry_allocation_coin c ON c.entitlement_id=e.id JOIN expiry_allocation a ON a.id=c.allocation_id \
					WHERE e.vtxo_id=$1 AND c.active AND a.status<>'conflicted')", &[&htlc.id().to_string()]).await?
					.get::<_, bool>(0))).await.unwrap();
				if decided {
					break;
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the ordinary matured source must acquire an actual allocation");
	}
}

/// An issued refresh can be cancelled before its forfeit binds. The
/// product's participation-removal method must not erase the old unclaimed
/// output's ownership. This tests settlement proof, not a payout race.
#[tokio::test]
async fn expiry_cancelled_unclaimed_output_keeps_owner_after_participation_removed() {
	let ctx = TestContext::new("server/expiry_cancelled_unclaimed_tombstone").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool().create().await;
	let wallet = ctx.bark("wallet", &srv).funded(sat(500_000)).create().await;
	let originals = wallet.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	ctx.refresh_all(&srv, &[&wallet]).await;
	let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
	let (participation, hash, leaf) = db.read(async |t| {
		let row = t.query_one("
			SELECT id, unlock_hash, forfeited_at IS NOT NULL AS forfeited
			FROM round_participation
			WHERE round_id IS NOT NULL
			ORDER BY id DESC
			LIMIT 1
		", &[]).await?;
		assert!(!row.get::<_, bool>("forfeited"));
		let hash: ark::tree::signed::UnlockHash = row.get::<_, &str>("unlock_hash").parse()?;
		let ids = t.query("
			SELECT vtxo_id
			FROM vtxo
			WHERE spend_state = 'unclaimed'
			ORDER BY id
		", &[]).await?
			.into_iter().map(|r| r.get::<_, &str>(0).parse::<ark::VtxoId>()).collect::<Result<Vec<_>, _>>()?;
		let states = t.get_server_vtxos_by_id(&ids).await?;
		let leaf = states.into_iter().find(|state| state.vtxo.clone().try_into_user_vtxo()
			.is_ok_and(|user| user.unlock_hash() == Some(hash))).expect("actual round must store its unclaimed output").vtxo;
		Ok((row.get::<_, i64>("id"), hash, leaf))
	}).await.unwrap();
	assert!(db.write(async |t| t.cancel_expiry_participations(&[participation]).await).await.unwrap());
	assert!(db.write(async |t| t.remove_round_participation(hash).await).await.unwrap());
	let remaining = db.read(async |t| Ok(t.query_one("
		SELECT COUNT(*)
		FROM round_participation
		WHERE id = $1
	", &[&participation]).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(remaining, 0);
	let funding = ctx.bitcoind().await_transaction(leaf.chain_anchor().txid).await;
	let root = leaf.to_bare();
	let snapshot = db.read(async |t| t.backing_snapshot(&[root.clone()]).await).await.unwrap();
	let fact = snapshot.regular.iter().find(|f| f.state.vtxo_id == root.id()).unwrap();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let proposal = db.read(async |t| t.propose_expiry_ownership(fact, tip, Some(tip)).await).await.unwrap();
	assert!(matches!(proposal.disposition, server::database::expiry_ownership::OwnershipDisposition::Operator));
	assert!(matches!(proposal.evidence,
		Some(server::database::expiry_ownership::OwnershipEvidence::Exchange {
			participation_id, ref unlock_hash, forfeited: false
		}) if participation_id == participation && unlock_hash == &hash.to_string()
	));
	let inputs = db.read(async |t| t.get_user_vtxos_by_id(&originals).await).await.unwrap();
	assert!(inputs.iter().all(|input| input.spend_state == server::database::SpendState::Spendable));
	assert_eq!(inputs.iter().map(|input| input.vtxo.amount().to_sat()).sum::<u64>(), 200_000);
}
