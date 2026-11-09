use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use axum::Router;
use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::Preimage;
use ark::fees::VtxoFeeInfo;
use ark::{ProtocolEncoding, VtxoPolicy};
use ark_testing::{btc, sat, TestContext};
use bark::actions::offboard::Progress;
use bark::persist::{BarkPersister, sqlite::SqliteClient};
use bark::vtxo::{VtxoLockHolder, VtxoState};
use ark_testing::daemon::captaind::{self, ArkClient};
use bitcoin::{Transaction, Txid};
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::BlockHeight;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::{Db, Tx};
use server_rpc::protos;
use tokio::sync::{Notify, oneshot};

/// Park an SDK request after prepare/signing, before the server's
/// durable registration. The request uses signed transactions.
#[derive(Clone)]
struct HoldFinish {
	arrived: Arc<Mutex<Option<oneshot::Sender<protos::FinishOffboardRequest>>>>,
	release: Arc<Notify>,
	response: Arc<Mutex<Option<Result<protos::FinishOffboardResponse, tonic::Status>>>>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for HoldFinish {
	async fn finish_offboard(
		&self, upstream: &mut ArkClient, request: protos::FinishOffboardRequest,
	) -> Result<protos::FinishOffboardResponse, tonic::Status> {
		let arrived = self.arrived.lock().unwrap().take();
		if let Some(arrived) = arrived {
			arrived.send(request.clone()).expect("race controller still alive");
			self.release.notified().await;
		}
		let result = upstream.finish_offboard(request).await.map(tonic::Response::into_inner);
		self.response.lock().unwrap().get_or_insert_with( || result.clone());
		result
	}
}

/// An undelivered offboard must keep its backing reserved until Bitcoin
/// confirms the customer payment, even after the client loses its reply.
#[tokio::test]
async fn unbroadcast_offboard_remains_customer_liability() {
	let ctx = TestContext::new("server/unbroadcast_offboard_remains_customer_liability").await;
	let upstream_url = ctx.bitcoind().rpc_url();
	let http_client = reqwest::Client::builder().no_proxy().build().unwrap();
	let reject_broadcast = Arc::new(AtomicBool::new(false));
	let rejected = Arc::new(AtomicUsize::new(0));
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let proxy_url = format!("http://{}", listener.local_addr().unwrap());
	let app = Router::new().route("/", post({
		let reject_broadcast = reject_broadcast.clone();
		let rejected = rejected.clone();
		move |headers: HeaderMap, body: Bytes| {
			let (reject_broadcast, rejected, upstream_url, http_client) =
				(reject_broadcast.clone(), rejected.clone(), upstream_url.clone(), http_client.clone());
			async move {
				let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
				if request["method"] == "sendrawtransaction" && reject_broadcast.load(Ordering::SeqCst) {
					rejected.fetch_add(1, Ordering::SeqCst);
					let error = serde_json::json!({"result": null, "error": {
						"code": -26, "message": "test: captaind broadcast unavailable"
					}, "id": request["id"]});
					return (StatusCode::OK, Bytes::from(serde_json::to_vec(&error).unwrap()));
				}
				let response = http_client.post(upstream_url)
					.header(header::AUTHORIZATION, headers[header::AUTHORIZATION].clone())
					.header(header::CONTENT_TYPE, "application/json")
					.body(body).send().await.unwrap();
				(response.status(), response.bytes().await.unwrap())
			}
		}
	})).layer(axum::extract::DefaultBodyLimit::disable());
	let _http_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(move |cfg| {
			cfg.bitcoind.url = proxy_url;
			cfg.expiry_payouts = true;
			cfg.vtxo_lifetime = BlockDelta::new(24);
		}).watchmand().funded(btc(1)).create().await;
	#[derive(Clone)]
	struct LoseFinishedReply(Arc<Mutex<Option<Vec<u8>>>>);
	#[async_trait::async_trait]
	impl captaind::proxy::ArkRpcProxy for LoseFinishedReply {
		async fn finish_offboard(&self, upstream: &mut ArkClient, req: protos::FinishOffboardRequest)
			-> Result<protos::FinishOffboardResponse, tonic::Status>
		{
			let result = upstream.finish_offboard(req).await?.into_inner();
			*self.0.lock().unwrap() = Some(result.signed_offboard_tx);
			Err(tonic::Status::unavailable("test: signed offboard reply lost"))
		}
	}
	let signed_reply = Arc::new(Mutex::new(None));
	let proxy = srv.start_proxy_no_mailbox(LoseFinishedReply(signed_reply.clone())).await;
	let owner = ctx.bark_sdk("owner", &proxy).cfg(|cfg| cfg.daemon_manual_sync = true)
		.boarded(sat(100_000)).create().await;
	let coin = owner.spendable_vtxos().await.unwrap().pop().unwrap();
	let coin_id = coin.id();
	let destination = ctx.bitcoind().get_new_address();
	let watchman_funding = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(watchman_funding, sat(100_000)).await;
	ctx.generate_blocks(2).await;

	reject_broadcast.store(true, Ordering::SeqCst);
	owner.offboard_vtxos([coin_id], destination.clone()).await.expect_err("client response is lost");
	let raw = signed_reply.lock().unwrap().clone().expect("server finished the offboard");
	let offboard: Transaction = deserialize(&raw).unwrap();
	let offboard_txid = offboard.compute_txid();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let persisted = db.read(async |tx| Ok(tx.query_one(
		"SELECT wallet_commit FROM offboards WHERE txid=$1", &[&offboard_txid.to_string()],
	).await?.get::<_, bool>(0))).await.unwrap();
	assert!(persisted, "server committed the offboard before on-chain delivery");
	let source = db.read(async |tx| tx.get_user_vtxos_by_id(&[coin_id]).await).await.unwrap();
	assert_eq!(source[0].offboarded_in, Some(offboard_txid));
	assert!(rejected.load(Ordering::SeqCst) > 0);
	assert!(ctx.bitcoind().sync_client().custom_get_raw_transaction_info(offboard_txid, None).unwrap().is_none());

	let current = ctx.bitcoind().get_block_count().await as u32;
	let mature = ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(mature).await;
	srv.watchmand().wait_for_sync_height(mature).await;
	for _ in 0..3 {
		srv.watchmand().trigger_sweep().await;
		ctx.generate_blocks(1).await;
	}
	let state = db.read(async |tx| Ok(tx.query_one(
		"SELECT COUNT(*) FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1",
		&[&coin_id.to_string()],
	).await?.get::<_, i64>(0))).await.unwrap();
	assert_eq!(state, 0, "unpaid offboard must not release customer backing");
	assert_eq!(ctx.bitcoind().get_received_by_address(&destination), sat(0));
	assert!(ctx.bitcoind().sync_client().custom_get_raw_transaction_info(offboard_txid, None).unwrap().is_none());

	// Once the signed payment is rescued, the same source may be released.
	reject_broadcast.store(false, Ordering::SeqCst);
	ctx.bitcoind().sync_client().send_raw_transaction(&offboard).unwrap();
	let delivered = offboard.output.iter().find(|out| out.script_pubkey == destination.script_pubkey()).unwrap().value;
	let tip = ctx.generate_blocks(6).await;
	srv.wait_for_sync_height(tip).await;
	srv.watchmand().wait_for_sync_height(tip).await;
	assert_eq!(ctx.bitcoind().get_received_by_address(&destination), delivered);
	let confirmed = db.read(async |tx| Ok(tx.query_one(
		"SELECT confirmed_at_height FROM nursery_tx WHERE txid=$1",
		&[&offboard_txid.to_string()],
	).await?.get::<_, Option<i32>>(0))).await.unwrap();
	assert!(confirmed.is_some());
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			srv.watchmand().trigger_sweep().await;
			let owner = db.read(async |tx| Ok(tx.query_opt(
				"SELECT c.disposition FROM expiry_allocation_coin c \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1", &[&coin_id.to_string()],
			).await?.map(|row| row.get::<_, String>(0)))).await.unwrap();
			if let Some(owner) = owner {
				assert_eq!(owner, "operator", "confirmed payment may release backing once");
				break;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}).await.expect("confirmed offboard did not release its backing");
}

pub(super) async fn wait_for_query_lock(tx: &Tx<'_>, prefix: &str) -> anyhow::Result<()> {
	tokio::time::timeout(Duration::from_secs(15), async {
		loop {
			tx.query_one("
				SELECT pg_stat_clear_snapshot()
			", &[]).await?;
			let waiting = tx.query_one("
				SELECT EXISTS(
					SELECT 1
					FROM pg_stat_activity
					WHERE datname=current_database()
					AND wait_event_type='Lock'
					AND ltrim(query, chr(9) || chr(10) || chr(13) || ' ') LIKE $1)
			", &[&prefix]).await?.get::<_, bool>(0);
			if waiting { return Ok::<_, anyhow::Error>(()); }
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	}).await.expect("real competing operation never reached its PostgreSQL row lock")
}

async fn published_payout(db: &Db, id: ark::VtxoId, message: &str) -> Transaction {
	tokio::time::timeout(Duration::from_secs(20), async {
		loop {
			if let Some((_, parent)) = super::expiry_test::published_payout(db, id).await { break parent; }
			tokio::time::sleep(Duration::from_millis(25)).await;
		}
	}).await.expect(message)
}

async fn offboard_race(offboard_wins: bool, after_analysis: bool) {
	let name = match (offboard_wins, after_analysis) {
		(true, false) => "server/expiry_native_offboard_wins",
		(true, true) => "server/expiry_native_offboard_after_analysis",
		(false, _) => "server/expiry_native_decision_wins",
	};
	let ctx = TestContext::new(name).await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().funded(btc(1))
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); }).watchmand().create().await;
	let (send, arrived) = oneshot::channel();
	let gate = HoldFinish {
		arrived: Arc::new(Mutex::new(Some(send))), release: Arc::new(Notify::new()),
		response: Arc::new(Mutex::new(None))
	};
	let proxy = srv.start_proxy_no_mailbox(gate.clone()).await;
	// This actor follows the test's explicit RPC/height schedule.
	let builder = ctx.bark_sdk("owner", &proxy).cfg(move |config| {
		config.daemon_manual_sync = true;
	});
	let wallet = Arc::new(builder.boarded(sat(100_000)).create().await);
	let coin = wallet.spendable_vtxos().await.unwrap().pop().unwrap();
	let id = coin.id();
	let destination = wallet.fallback_script();
	assert_eq!(coin.amount(), sat(100_000));
	let offboard_address = ctx.bitcoind().get_new_address();
	let funding = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(funding, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let sender = wallet.clone();
	let address = offboard_address.clone();
	let operation = tokio::spawn(async move { sender.offboard_vtxos([id], address).await });
	let request = tokio::time::timeout(Duration::from_secs(15), arrived).await.unwrap().unwrap();
	let offboard_txid = Txid::from_slice(&request.offboard_txid).unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	// The old watchman view prevents a decision while Core and captaind
	// reach expiry. No chain acknowledgement or financial row is fabricated.
	srv.watchmand().stop().await.unwrap();
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(coin.expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	if offboard_wins && after_analysis {
		db.write(async |tx| {
			// The decision analyses the unspent coin, then waits for this lock
			// before its checked write while the offboard commits in full.
			tx.execute("SELECT pg_advisory_xact_lock($1,$2)",
				&[&5_i32, &i32::try_from(coin.expiry_height().to_u32())?]).await?;
			srv.watchmand().start().await.unwrap();
			wait_for_query_lock(tx, "SELECT pg_advisory_xact_lock($1,$2)").await?;
			gate.release.notify_one();
			while gate.response.lock().unwrap().is_none() { tokio::time::sleep(Duration::from_millis(20)).await; }
			Ok(())
		}).await.unwrap();
	} else if offboard_wins {
		db.write(async |tx| {
			tx.query_one("
				SELECT vtxo_id
				FROM vtxo
				WHERE vtxo_id=$1
				FOR UPDATE
			", &[&id.to_string()]).await?;
			gate.release.notify_one();
			wait_for_query_lock(tx, "SELECT vtxo_id FROM vtxo WHERE vtxo_id = ANY%").await?;
			// The native offboard is first in the real row-lock queue. The
			// completed watchman now permits a real allocation to queue too.
			srv.watchmand().start().await.unwrap();
			wait_for_query_lock(tx, "SELECT vtxo_id FROM vtxo WHERE vtxo_id=ANY($1) ORDER BY vtxo_id FOR UPDATE").await?;
			Ok(())
		}).await.unwrap();
	}
	if offboard_wins {
		let actual = tokio::time::timeout(Duration::from_secs(20), operation).await.unwrap().unwrap().unwrap().txid;
		assert_eq!(actual, offboard_txid);
		let response = gate.response.lock().unwrap().clone().unwrap().unwrap();
		let signed: Transaction = deserialize(&response.signed_offboard_tx).unwrap();
		assert_eq!(ctx.bitcoind().await_transaction(actual).await, signed);
		let delivered = signed.output.iter().find(|o| o.script_pubkey == offboard_address.script_pubkey()).unwrap().value;
		let fee = db.read(async |tx| Ok(tx.query_one("
			SELECT user_fee_sat
			FROM offboards
			WHERE txid=$1
		", &[&actual.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(delivered + sat(u64::try_from(fee).unwrap()), coin.amount());
		ctx.generate_blocks(6).await;
		assert_eq!(ctx.bitcoind().get_received_by_address(&offboard_address), delivered);
		let customer = db.read(async |tx| Ok(tx.query_one("
			SELECT COUNT(*)
			FROM expiry_allocation_coin c
			JOIN expiry_entitlement e ON e.id=c.entitlement_id
			WHERE e.vtxo_id=$1
			AND c.disposition='customer'
		", &[&id.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(customer, 0, "the binding offboard must not also receive a fallback payment");
	} else {
		srv.watchmand().start().await.unwrap();
		let parent = published_payout(&db, id, "actual payout decision did not publish").await;
		gate.release.notify_one();
		let error = tokio::time::timeout(Duration::from_secs(20), operation).await.unwrap().unwrap().unwrap_err();
		assert_eq!(bark::FallbackRefusal::from_error(&error), Some(bark::FallbackRefusal::DecidedForFallback));
		let status = gate.response.lock().unwrap().clone().unwrap().unwrap_err();
		assert_eq!(status.code(), tonic::Code::InvalidArgument);
		assert_eq!(status.metadata().get("abandon-reason").unwrap(), "decided_for_fallback");
		let state = db.read(async |tx| tx.get_user_vtxos_by_id(&[id]).await).await.unwrap();
		assert_eq!(state[0].offboarded_in, None);
		let persisted = db.read(async |tx| Ok(tx.query_one("
			SELECT COUNT(*)
			FROM offboards
			WHERE txid=$1
		", &[&offboard_txid.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(persisted, 0, "the actual registration transaction must roll back its offboard insert");
		assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
		let (index, output) = parent.output.iter().enumerate().find(|(_, o)| o.script_pubkey == destination).unwrap();
		assert_eq!(output.value, coin.amount() - sat(2_000));
		ctx.generate_blocks(6).await;
		assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
		assert_eq!(ctx.bitcoind().get_received_by_address(&offboard_address), bitcoin::Amount::ZERO);

	}
}

#[tokio::test]
async fn expiry_decision_wins_over_prepared_native_offboard() {
	offboard_race(false, false).await;
}

#[tokio::test]
async fn expiry_registered_native_offboard_wins_over_waiting_decision() {
	offboard_race(true, false).await;
}

/// The decision's checked write refuses a plan whose coin was offboarded
/// after the analysis read it.
#[tokio::test]
async fn expiry_decision_aborts_after_offboard_commits_during_analysis() {
	offboard_race(true, true).await;
}

/// A user returning after backing expiry can close a never-initiated grant
/// before its later HTLC deadline. This needs no configured Lightning node.
#[tokio::test]
async fn expiry_uninitiated_grant_can_revoke_after_backing_expiry() {
	let ctx = TestContext::new("server/expiry_post_backing_grant_revocation").await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime = BlockDelta::new(24); }).watchmand().create().await;
	// Abandonment means this actor runs no automatic protection task.
	let wallet = ctx.bark_sdk("owner", &srv)
		.cfg(|config| config.daemon_manual_sync = true).boarded(sat(100_000)).create().await;
	let input = wallet.spendable_vtxos().await.unwrap().pop().unwrap();
	let input = wallet.get_full_vtxo(input.id()).await.unwrap();
	let key = wallet.get_vtxo_key(&input).await.unwrap();
	let destination = wallet.fallback_script();
	let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
	assert!(deadline > input.expiry_height());
	let hash = Preimage::from([91;32]).compute_payment_hash();
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input.clone()],
		VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).unwrap()
		.generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
		parts: super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
	}).await.unwrap().into_inner();
	let grants = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(grants.len(), 1);
	wallet.register_vtxo_transactions_with_server(&grants).await.unwrap();
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(input.expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	srv.watchmand().wait_for_sync_height(expired).await;
	assert!(expired < deadline);
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	assert_eq!(db.read(async |tx| Ok(tx.query_one("
		SELECT COUNT(*)
		FROM lightning_payment_attempt_htlc_vtxo
		WHERE vtxo_id=$1
	", &[&grants[0].id().to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([grants[0].clone()],
		VtxoPolicy::new_pubkey(key.public_key())).unwrap().generate_user_nonces(&[key]).unwrap();
	let response = rpc.request_lightning_pay_htlc_revocation(
		super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await,
	).await.unwrap().into_inner();
	let refunds = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(refunds.iter().map(|coin| coin.amount()).sum::<bitcoin::Amount>(), input.amount());
	wallet.register_vtxo_transactions_with_server(&refunds).await.unwrap();
	let funding = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(funding, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let parent = published_payout(&db, refunds[0].id(),
		"genuine post-expiry revocation did not permit its actual fallback payout").await;
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
	let (index, output) = parent.output.iter().enumerate().find(|(_, o)| o.script_pubkey == destination).unwrap();
	assert_eq!(output.value, input.amount() - sat(2_000));
	ctx.generate_blocks(6).await;
	assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
	assert!((ctx.bitcoind().get_block_count().await as u32) < deadline.to_u32());
}

/// One expired, never-finished two-input intent is superseded by a real
/// ordinary spend of lower-ID B and canonical customer fallback of A.
#[tokio::test]
async fn mixed_terminal_offboard_retires_after_lower_id_ordinary_spend_and_fallback() {
	const SESSION_TIMEOUT: Duration = Duration::from_secs(5);
	let ctx=TestContext::new("server/mixed_terminal_offboard_lower_id_ordinary_spend").await;
	let srv=ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().funded(btc(1))
		.cfg(|cfg| { cfg.expiry_payouts = true; cfg.vtxo_lifetime=BlockDelta::new(24); cfg.offboard_session_timeout=SESSION_TIMEOUT; })
		.watchmand().create().await;
	srv.watchmand().stop().await.unwrap();
	#[derive(Clone)]
	struct DropFirstFinish {
		calls:Arc<AtomicUsize>,
		finish_result:Arc<Mutex<Option<Result<protos::FinishOffboardResponse,tonic::Status>>>>,
		prepare_result:Arc<Mutex<Option<Result<protos::PrepareOffboardResponse,tonic::Status>>>>,
		prepare_ids:Arc<Mutex<Vec<Vec<u8>>>>,
	}
	#[async_trait::async_trait]
	impl captaind::proxy::ArkRpcProxy for DropFirstFinish {
		async fn prepare_offboard(&self,upstream:&mut ArkClient,request:protos::PrepareOffboardRequest)
			->Result<protos::PrepareOffboardResponse,tonic::Status>
		{
			*self.prepare_ids.lock().unwrap()=request.input_vtxo_ids.clone();
			let result=upstream.prepare_offboard(request).await.map(tonic::Response::into_inner);
			*self.prepare_result.lock().unwrap()=Some(result.clone()); result
		}
		async fn finish_offboard(&self,upstream:&mut ArkClient,request:protos::FinishOffboardRequest)
			->Result<protos::FinishOffboardResponse,tonic::Status>
		{
			if self.calls.fetch_add(1,Ordering::SeqCst)==0 {
				return Err(tonic::Status::internal("proxy dropped the original finish_offboard request"));
			}
			let result=upstream.finish_offboard(request).await.map(tonic::Response::into_inner);
			*self.finish_result.lock().unwrap()=Some(result.clone()); result
		}
	}
	let gate=DropFirstFinish {calls:Arc::new(AtomicUsize::new(0)),finish_result:Arc::new(Mutex::new(None)),
		prepare_result:Arc::new(Mutex::new(None)),prepare_ids:Arc::new(Mutex::new(Vec::new()))};
	let proxy=srv.start_proxy_no_mailbox(gate.clone()).await;
	let owner=ctx.bark_sdk("owner",&proxy).funded(sat(500_000)).boarded(sat(100_000)).boarded(sat(100_000))
		.cfg(|cfg| { cfg.daemon_manual_sync=true; cfg.vtxo_refresh_expiry_threshold=BlockDelta::new(4);
			cfg.vtxo_exit_margin=BlockDelta::new(2); }).create().await;
	owner.stop_daemon_wait().await.unwrap();
	let mut coins=owner.spendable_vtxos().await.unwrap(); coins.sort_by_key(|coin|coin.id());
	assert_eq!(coins.len(),2); assert!(coins.iter().all(|coin|coin.amount()==sat(100_000)));
	let b=owner.get_full_vtxo(coins[0].id()).await.unwrap();
	let a=owner.get_full_vtxo(coins[1].id()).await.unwrap();
	assert!(b.id()<a.id(),"ordinary-Spent B must be the FIRST server input check");
	let ids=vec![b.id(),a.id()];
	let fallback=owner.fallback_script();
	let offboard_address=ctx.bitcoind().get_new_address();
	let funding=srv.watchmand().wait_wallet_address().await;
	let sponsor=ctx.bitcoind().fund_addr(funding.clone(),sat(100_000)).await;
	ctx.generate_blocks(2).await;
	let postgres=srv.config().postgres.clone();
	let db=Db::connect(&postgres).await.unwrap();
	let wallet_db=SqliteClient::open(ctx.datadir.join("owner/db.sqlite")).unwrap();
	let mut expired=srv.subscribe_log::<server_log::OffboardSessionTimeout>();
	let error=Box::pin(owner.offboard_vtxos(ids.clone(),offboard_address.clone())).await.unwrap_err();
	assert!(format!("{error:#}").contains("proxy dropped the original finish_offboard request"));
	assert_eq!(gate.calls.load(Ordering::SeqCst),1);
	let prepared=owner.pending_offboards().await.unwrap().pop().unwrap();
	let (offboard_txid,movement)=match &prepared.progress {
		Progress::OffboardTxPrepared {offboard_tx,movement_id,offboard_vtxo_ids,..}=>{
			assert_eq!(*offboard_vtxo_ids,ids); (offboard_tx.compute_txid(),*movement_id)
		}, _=>panic!("the original whole offboard must be signed/prepared before the dropped finish"),
	};
	assert!(matches!(&prepared.kind,bark::actions::offboard::OffboardKind::OffboardWhole {input_vtxo_ids} if *input_vtxo_ids==ids));
	assert_eq!(prepared.onchain_output_amount+prepared.committed_fee,sat(200_000));
	for id in &ids {
		assert_eq!(wallet_db.get_wallet_vtxo(*id).await.unwrap().unwrap().state,
			VtxoState::Locked {holder:Some(VtxoLockHolder::Action {id:prepared.id.clone()})});
	}
	// Observe the real server session expiration, not merely a slept interval.
	let event=tokio::time::timeout(Duration::from_secs(20),async {
		loop {let event=expired.recv().await.unwrap(); if event.offboard_txid==offboard_txid {break event;}}
	}).await.expect("the genuine unfinished offboard session must expire and release its server guards");
	let mut event_ids=event.vtxos; event_ids.sort_unstable(); assert_eq!(event_ids,ids);
	assert!(ctx.bitcoind().sync_client().custom_get_raw_transaction_info(offboard_txid,None).unwrap().is_none());
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM offboards WHERE txid=$1",
		&[&offboard_txid.to_string()]).await?.get::<_,i64>(0))).await.unwrap(),0);
	owner.sync_pending_offboards().await.unwrap();
	let lost=gate.finish_result.lock().unwrap().clone().unwrap().unwrap_err();
	assert_eq!(lost.code(),tonic::Code::InvalidArgument); assert!(lost.message().contains("unknown offboard txid"));
	let retry=owner.offboard_checkpoint(&prepared.id).await.unwrap().unwrap();
	assert!(matches!(&retry.progress,Progress::ReadyForOffboard {offboard_vtxo_ids,prior_txid:Some(txid)}
		if *offboard_vtxo_ids==ids && *txid==offboard_txid));
	let mut expected=prepared.clone(); expected.progress=retry.progress.clone();
	assert!(retry==expected,"only the genuine lost-session progress may change");
	let mnemonic=tokio::fs::read_to_string(ctx.datadir.join("owner/mnemonic")).await.unwrap()
		.trim().parse::<bip39::Mnemonic>().unwrap();
	let restored=ctx.bark_sdk("restored",&srv).mnemonic(mnemonic)
		.cfg(|cfg| {cfg.daemon_manual_sync=true; cfg.vtxo_refresh_expiry_threshold=BlockDelta::new(4);
			cfg.vtxo_exit_margin=BlockDelta::new(2);}).create().await;
	restored.stop_daemon_wait().await.unwrap();
	let recovered=restored.spendable_vtxos().await.unwrap();
	assert_eq!(recovered.len(),2); assert!(ids.iter().all(|id|recovered.iter().any(|coin|coin.id()==*id)));
	assert_eq!(restored.mailbox_keypair().public_key(),owner.mailbox_keypair().public_key());
	for coin in [&a,&b] {
		assert_eq!(restored.get_full_vtxo(coin.id()).await.unwrap().serialize(),coin.serialize());
		assert_eq!(restored.get_vtxo_key(coin).await.unwrap().public_key(),coin.user_pubkey());
	}
	// Ordinary SDK coin control on ONLY the restored device: leave A aside
	// so its public send must consume lower-ID B, without changing original holders.
	restored.lock_vtxos([a.id()],None).await.unwrap();
	let recipient=ctx.bark_sdk("recipient",&srv).cfg(|cfg| {cfg.daemon_manual_sync=true;
		cfg.vtxo_refresh_expiry_threshold=BlockDelta::new(4);cfg.vtxo_exit_margin=BlockDelta::new(2);}).create().await;
	recipient.stop_daemon_wait().await.unwrap();
	let address=recipient.new_address().await.unwrap();
	let sent=Box::pin(restored.send_arkoor_payment(&address,sat(100_000))).await.unwrap();
	Box::pin(recipient.sync()).await;
	assert_eq!(restored.get_vtxo_by_id(b.id()).await.unwrap().state,VtxoState::Spent);
	assert_eq!(restored.get_vtxo_by_id(a.id()).await.unwrap().state,VtxoState::Locked {holder:None});
	let send=restored.history().await.unwrap().into_iter().find(|m|m.id==sent).unwrap();
	assert_eq!(send.status,bark::movement::MovementStatus::Successful);
	assert_eq!(send.effective_balance,bitcoin::SignedAmount::from_sat(-100_000)); assert_eq!(send.offchain_fee,sat(0));
	assert_eq!(recipient.balance().await.unwrap().spendable,sat(100_000));
	let received=recipient.spendable_vtxos().await.unwrap(); assert_eq!(received.len(),1);
	let received=recipient.get_full_vtxo(received[0].id()).await.unwrap();
	assert_eq!(received.chain_anchor(),b.chain_anchor());
	assert_eq!(received.expiry_height(),b.expiry_height());
	let received_history=recipient.history().await.unwrap();
	assert_eq!(received_history.iter().filter(|m|m.subsystem.name=="bark.arkoor").count(),1);
	for id in &ids {assert_eq!(wallet_db.get_wallet_vtxo(*id).await.unwrap().unwrap().state,
		VtxoState::Locked {holder:Some(VtxoLockHolder::Action {id:prepared.id.clone()})});}
	assert!(owner.offboard_checkpoint(&prepared.id).await.unwrap().is_some_and(|cp|cp==retry));

	// B inherits its original expiry. Schedule the one ordinary refresh late
	// enough that its real tip+lifetime expiry covers A's six-confirmation tail.
	let refresh_tip=a.expiry_height().saturating_sub(srv.config().vtxo_lifetime)+BlockDelta::new(8);
	let current=ctx.bitcoind().get_block_count().await as u32;
	if current<refresh_tip.to_u32() {ctx.generate_blocks(refresh_tip.to_u32()-current).await;}
	let tip=BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
	assert!(tip<b.expiry_height());
	assert!(tip+BlockDelta::new(ark_testing::constants::ROUND_CONFIRMATIONS.try_into().unwrap())<a.expiry_height());
	recipient.chain().invalidate_caches().await;
	let quote=recipient.ark_info().await.unwrap().unwrap().fees.refresh
		.calculate([VtxoFeeInfo::from_vtxo_and_tip(&received,tip)]).unwrap();
	let mut finished=srv.subscribe_log::<server_log::RoundFinished>();
	Box::pin(recipient.refresh_vtxos_delegated([received.id()])).await.unwrap().unwrap();
	srv.trigger_round().await;
	let round=tokio::time::timeout(Duration::from_secs(30),finished.recv()).await.unwrap().unwrap();
	let _ = ctx.bitcoind().await_transaction(round.txid).await;
	ctx.generate_blocks(ark_testing::constants::ROUND_CONFIRMATIONS).await;
	tokio::time::timeout(Duration::from_secs(30),async {
		loop {
			recipient.chain().invalidate_caches().await;
			Box::pin(recipient.sync()).await;
			if recipient.pending_round_states().await.unwrap().is_empty() {break;}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}).await.expect("the one genuine recipient refresh must confirm under its existing bound");
	let live=recipient.spendable_vtxos().await.unwrap(); assert_eq!(live.len(),1);
	let live=recipient.get_full_vtxo(live[0].id()).await.unwrap();
	assert_eq!(live.amount()+quote,sat(100_000));
	assert!(live.expiry_height()>a.expiry_height()+BlockDelta::new(7),
		"the real replacement expiry must cover A expiry+1 and its original six confirmations");
	let refreshes=recipient.history().await.unwrap().into_iter().filter(|m|
		m.subsystem.is_subsystem(bark::subsystem::Subsystem::ROUND)
		&& m.status==bark::movement::MovementStatus::Successful && m.offchain_fee==quote
		&& m.effective_balance == -quote.to_signed().unwrap()).collect::<Vec<_>>();
	assert_eq!(refreshes.len(),1,"one actual quoted refresh fee");
	let current=ctx.bitcoind().get_block_count().await as u32;
	assert!(current<a.expiry_height().to_u32(),"the single legal refresh must finish before A matures");
	let height=ctx.generate_blocks(a.expiry_height().to_u32()+1-current).await;
	srv.watchmand().start().await.unwrap();
	super::expiry_test::rebuilt(&ctx,&srv,&db).await;
	let parent=published_payout(&db,a.id(),"A's actual ordinary fallback must publish").await;
	assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await,parent);
	let chain=ctx.bitcoind().sync_client();
	// Both expired backing roots are batched: A pays its customer, while B's
	// completed ordinary refresh lets the operator reclaim its old backing.
	assert_eq!(parent.input.len(),2);
	let points=parent.input.iter().map(|input|input.previous_output).collect::<std::collections::HashSet<_>>();
	assert_eq!(points,[a.chain_anchor(),b.chain_anchor()].into_iter().collect());
	let mut backing_amount=sat(0);
	for coin in [&a,&b] {
		let backing=ctx.bitcoind().await_transaction(coin.chain_anchor().txid).await;
		backing_amount+=backing.output[coin.chain_anchor().vout as usize].value;
	}
	let original_board_fee=backing_amount.checked_sub(a.amount()+b.amount()).unwrap();
	let miner_fee=super::expiry_test::actual_fee(&ctx,&parent).await;
	assert!(miner_fee>sat(0));
	assert_eq!(parent.output.iter().map(|o|o.value).sum::<bitcoin::Amount>()+miner_fee,backing_amount);
	let (index,output)=parent.output.iter().enumerate().find(|(_,o)|o.script_pubkey==fallback).unwrap();
	assert_eq!(output.value,sat(98_000));
	let operator=srv.watchmand().config().sweep_address.clone().unwrap().require_network(bitcoin::Network::Regtest).unwrap().script_pubkey();
	assert_ne!(operator,fallback);
	assert_eq!(parent.output.iter().filter(|o|o.script_pubkey==operator).map(|o|o.value).sum::<bitcoin::Amount>()+miner_fee,
		b.amount()+original_board_fee+sat(2_000));
	assert_eq!(parent.output.len(),2);
	assert_eq!(parent.version,bitcoin::transaction::Version::TWO);
	assert!(chain.get_tx_out(&parent.compute_txid(),index as u32,Some(true)).unwrap().is_some());
	// The real prepare checks ordinary-Spent B before the committed fallback input A.
	owner.sync_fallback_payouts().await.unwrap();
	owner.sync_pending_offboards().await.unwrap();
	let rejection=gate.prepare_result.lock().unwrap().clone().unwrap().unwrap_err();
	assert_eq!(*gate.prepare_ids.lock().unwrap(),ids.iter().map(|id|id.to_bytes().to_vec()).collect::<Vec<_>>());
	assert_eq!(rejection.code(),tonic::Code::InvalidArgument);
	assert!(rejection.message().contains(&b.id().to_string()) && rejection.message().contains("is not spendable"));
	assert!(rejection.metadata().get("abandon-reason").is_none(),"B must mask the later typed A refusal");
	assert_eq!(owner.get_vtxo_by_id(a.id()).await.unwrap().fallback_status,Some(bark::vtxo::FallbackStatus::Pending));
	let allocations=db.read(async |tx| Ok(tx.query("SELECT e.id,c.gross_sat,c.fee_sat,c.payout_sat,a.sweep_tx \
		FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id JOIN expiry_allocation_state a ON a.id=c.allocation_id \
		WHERE e.vtxo_id=$1 ORDER BY a.id",&[&a.id().to_string()]).await?.into_iter().map(|row|
		(row.get::<_,i64>(0),row.get::<_,i64>(1),row.get::<_,i64>(2),row.get::<_,i64>(3),row.get::<_,Vec<u8>>(4))).collect::<Vec<_>>())).await.unwrap();
	assert_eq!(allocations.len(),1); assert_eq!((allocations[0].1,allocations[0].2,allocations[0].3),(100_000,2_000,98_000));
	let operator_rows=db.read(async |tx| Ok(tx.query("SELECT c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat,a.sweep_txid \
		FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
		JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE e.vtxo_id=$1 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='operator'",
		&[&received.id().to_string()]).await?.into_iter().map(|row|(row.get::<_,i64>(0),row.get::<_,i64>(1),
			row.get::<_,i64>(2),row.get::<_,i64>(3),row.get::<_,String>(4))).collect::<Vec<_>>())).await.unwrap();
	assert_eq!(operator_rows,vec![(100_000,0,0,0,parent.compute_txid().to_string())]);
	let confirmed=ctx.generate_blocks(6).await;
	super::expiry_test::rebuilt(&ctx,&srv,&db).await;
	let actual=chain.get_tx_out(&parent.compute_txid(),index as u32,Some(true)).unwrap().unwrap();
	assert!(actual.confirmations>=6); assert_eq!(actual.value,sat(98_000));
	assert_eq!(actual.script_pub_key.script().unwrap(),fallback);
	owner.sync_fallback_payouts().await.unwrap();
	owner.sync_pending_offboards().await.unwrap();
	let remaining=owner.offboard_checkpoint(&prepared.id).await.unwrap();
	assert!(remaining.is_none(),"terminal fallback A plus durably ordinary-Spent B must retire the original whole intent");
	let old=wallet_db.get_movement_by_id(movement).await.unwrap();
	assert_eq!(old.status,bark::movement::MovementStatus::Canceled);
	assert_eq!(old.effective_balance,bitcoin::SignedAmount::ZERO); assert_eq!(old.offchain_fee,sat(0));
	for id in &ids {
		let coin=owner.get_vtxo_by_id(*id).await.unwrap(); assert_eq!(coin.state,VtxoState::Spent);
		assert!(owner.lock_vtxos([*id],None).await.is_err(),"neither original may be unlocked into Spendable");
	}
	assert!(owner.get_vtxo_by_id(b.id()).await.unwrap().fallback_status.is_none());
	db.read(async |tx| {
		let ordinary=tx.get_user_vtxos_by_id(&[b.id()]).await?.remove(0);
		assert_eq!(ordinary.spend_state,server::database::SpendState::Spent);
		assert!(ordinary.oor_spent_txid.is_some()); assert!(ordinary.offboarded_in.is_none());
		assert_eq!(tx.query_one("SELECT COUNT(*) FROM expiry_allocation_coin c JOIN expiry_entitlement e ON e.id=c.entitlement_id \
			WHERE e.vtxo_id=$1 AND c.disposition='customer'",&[&b.id().to_string()]).await?.get::<_,i64>(0),0);
		assert_eq!(tx.query_one("SELECT COUNT(*) FROM offboards WHERE txid=$1",&[&offboard_txid.to_string()]).await?.get::<_,i64>(0),0);
		Ok(())
	}).await.unwrap();
	assert_eq!(owner.get_vtxo_by_id(a.id()).await.unwrap().fallback_status,Some(bark::vtxo::FallbackStatus::Spent));
	assert!(owner.spendable_vtxos().await.unwrap().is_empty());
	assert_eq!(owner.get_full_vtxo(a.id()).await.unwrap().serialize(),a.serialize());
	assert_eq!(owner.get_full_vtxo(b.id()).await.unwrap().serialize(),b.serialize());
	assert_eq!(ctx.bitcoind().get_received_by_address(&offboard_address),sat(0));
	assert!(chain.custom_get_raw_transaction_info(offboard_txid,None).unwrap().is_none());
	assert_eq!(recipient.balance().await.unwrap().spendable,live.amount());
	assert_eq!(live.amount()+quote+actual.value+sat(2_000),sat(200_000));
	let sponsor_tx=ctx.bitcoind().await_transaction(sponsor).await;
	let sponsor_points=sponsor_tx.output.iter().enumerate().filter(|(_,o)|o.script_pubkey==funding.script_pubkey())
		.map(|(index,_)|bitcoin::OutPoint::new(sponsor,index as u32)).collect::<Vec<_>>();
	assert!(parent.input.iter().all(|input|!sponsor_points.contains(&input.previous_output)));
	assert!(sponsor_points.iter().all(|point|chain.get_tx_out(&point.txid,point.vout,Some(true)).unwrap().is_some()));
	let recipient_history=recipient.history().await.unwrap();
	let recipient_balance=recipient.balance().await.unwrap().summary();
	let history=owner.history().await.unwrap();
	let mempool=chain.get_raw_mempool().unwrap();
	owner.sync_pending_offboards().await.unwrap(); owner.sync_fallback_payouts().await.unwrap();
	assert!(owner.history().await.unwrap()==history,"terminal intent retirement is idempotent");
	assert!(owner.offboard_checkpoint(&prepared.id).await.unwrap().is_none());
	assert_eq!(chain.get_raw_mempool().unwrap(),mempool);
	let after=db.read(async |tx| Ok(tx.query("SELECT e.id,c.gross_sat,c.fee_sat,c.payout_sat,a.sweep_tx \
		FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id JOIN expiry_allocation_state a ON a.id=c.allocation_id \
		WHERE e.vtxo_id=$1 ORDER BY a.id",&[&a.id().to_string()]).await?.into_iter().map(|row|
		(row.get::<_,i64>(0),row.get::<_,i64>(1),row.get::<_,i64>(2),row.get::<_,i64>(3),row.get::<_,Vec<u8>>(4))).collect::<Vec<_>>())).await.unwrap();
	assert_eq!(after,allocations,"same original A entitlement, parent and fee once");
	assert!(recipient.history().await.unwrap()==recipient_history);
	assert_eq!(recipient.balance().await.unwrap().summary(),recipient_balance);
	assert_eq!(gate.calls.load(Ordering::SeqCst),2,"no new finish or replay of the unfinished transaction");
	assert_eq!(confirmed.to_u32(),height.to_u32()+6);
}
