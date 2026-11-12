use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use axum::Router;
use ark_testing::{btc, sat, TestContext};
use ark_testing::daemon::captaind::{self, ArkClient};
use bitcoin::{Transaction, Txid};
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash;
use bitcoin_ext::BlockDelta;
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
			// The decision analyses the unspent coin, then waits for its branch
			// rows before its checked write while the offboard commits in full.
			// KEY SHARE blocks only the decision's FOR UPDATE, not the offboard.
			tx.query("SELECT vtxo_id FROM vtxo WHERE anchor_point=$1 ORDER BY vtxo_id FOR KEY SHARE",
				&[&coin.chain_anchor().to_string()]).await?;
			srv.watchmand().start().await.unwrap();
			wait_for_query_lock(tx, "SELECT vtxo_id FROM vtxo WHERE vtxo_id=ANY($1) ORDER BY vtxo_id FOR UPDATE").await?;
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
		tokio::time::timeout(Duration::from_secs(20), operation).await.unwrap().unwrap().unwrap_err();
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

/// The decision's checked write refuses a plan whose coin was offboarded
/// after the analysis read it.
#[tokio::test]
async fn expiry_decision_aborts_after_offboard_commits_during_analysis() {
	offboard_race(true, true).await;
}
