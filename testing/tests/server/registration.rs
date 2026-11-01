use ark::{vtxo::Bare, vtxo::Full, ProtocolEncoding, Vtxo, VtxoPolicy};
use ark::board::BoardBuilder;
use bitcoin::{Address, Amount, Network, OutPoint};
use bitcoin::secp256k1::{Keypair, rand::thread_rng};
use bitcoin_ext::BlockHeight;
use bitcoin_ext::rpc::RpcApi;
use server::database::{Db, SpendState};
use server_rpc::{protos, RequestExt};

use ark_testing::{btc, sat, TestContext};
use ark_testing::constants::ROUND_CONFIRMATIONS;
use ark_testing::daemon::captaind::{self, ArkClient};
use ark_testing::exit::complete_exit;

/// The same vtxo twice in one registration request must be accepted as an
/// idempotent no-op. Without the handler-level dedupe the duplicate ids reach
/// the tree-update validation, which panics in debug builds via `debug_assert!`.
#[tokio::test]
async fn register_vtxo_transactions_accepts_duplicate_ids() {
	let ctx = TestContext::new("server/register_vtxo_transactions_accepts_duplicate_ids").await;
	let srv = ctx.captaind("server").funded(btc(10)).create().await;
	let bark = ctx.bark("bark", &srv).funded(sat(90_000)).create().await;
	let vtxo_ids = bark.board_and_confirm_and_register(&ctx, sat(80_000)).await;
	assert_eq!(vtxo_ids.len(), 1);

	// Board registration stored the fully-signed vtxo; replay it twice in
	// one request through the real RPC.
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let blob = db.read(async |t| {
		let row = t.query_one(
			"SELECT vtxo FROM vtxo WHERE vtxo_id = $1", &[&vtxo_ids[0].to_string()],
		).await?;
		Ok(row.get::<_, Vec<u8>>(0))
	}).await.unwrap();

	let mut client = srv.get_public_rpc().await;
	client.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: vec![blob.clone(), blob.clone()],
	}).await.expect("duplicate ids in one request should be accepted");

	// Over the cap is rejected, even when the vtxos would dedupe.
	let err = client.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: vec![blob; server_rpc::MAX_NB_REGISTER_VTXOS + 1],
	}).await.unwrap_err();
	assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// Acknowledges vtxo transaction registrations without forwarding them, so
/// the server keeps the resulting vtxos `unregistered` and holds their
/// transactions unsigned.
#[derive(Clone)]
struct DropRegistration;

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for DropRegistration {
	async fn register_vtxo_transactions(
		&self, _upstream: &mut ArkClient, _req: protos::RegisterVtxoTransactionsRequest,
	) -> Result<protos::Empty, tonic::Status> {
		Ok(protos::Empty {})
	}
}

/// Reject malformed amounts through the public entry before constructing its
/// transactions, without blocking a genuine unsigned-to-signed registration.
#[tokio::test]
async fn register_vtxo_transactions_refuses_overflowing_genesis() {
	let ctx = TestContext::new("server/register_vtxo_transactions_refuses_overflowing_genesis").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool().create().await;
	let sender = ctx.bark("sender", &srv).funded(sat(500_000)).create().await;
	sender.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	let proxy = srv.start_proxy_no_mailbox(DropRegistration).await;
	sender.set_ark_url(&proxy.address).await;
	let receiver = ctx.bark("receiver", &proxy.address).create().await;
	sender.send_oor(receiver.address().await, sat(50_000)).await;
	let ids = receiver.vtxo_ids().await;
	assert_eq!(ids.len(), 1);
	let signed = receiver.raw_vtxo(ids[0]).await;
	assert!(signed.has_all_witnesses());
	assert!(signed.amount() > Amount::ZERO);
	assert!(signed.exit_depth() > 0);

	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let stored = db.read(async |t| t.get_user_vtxo_by_id(signed.id()).await).await.unwrap();
	assert_eq!(stored.spend_state, SpendState::Unregistered);
	assert!(!stored.vtxo.has_all_witnesses());
	assert_eq!(stored.vtxo.to_bare().serialize(), signed.to_bare().serialize());
	let anchor = db.read(async |t| {
		Ok(t.get_virtual_transaction_by_txid(signed.chain_anchor().txid).await?
			.expect("existing chain anchor").signed_tx().cloned().expect("signed anchor"))
	}).await.unwrap();
	signed.validate(&anchor).unwrap();

	// The last eight bytes of a current genesis encoding are its final fee.
	// Keep every bare field, encoded ID and witness, but overflow the amount.
	let mut genesis = signed.serialize_genesis();
	let fee_start = genesis.len().checked_sub(8).unwrap();
	genesis[fee_start..].copy_from_slice(&u64::MAX.to_le_bytes());
	let malformed = Vtxo::<Full>::deserialize_with_genesis(
		&signed.to_bare().serialize(), &genesis,
	).unwrap();
	let malformed = Vtxo::<Full>::deserialize(&malformed.serialize()).unwrap();
	assert_eq!(malformed.id(), signed.id());
	assert_eq!(malformed.to_bare().serialize(), signed.to_bare().serialize());
	assert!(malformed.has_all_witnesses());
	assert_eq!(malformed.validate(&anchor),
		Err(ark::vtxo::VtxoValidationError::Invalid("onchain amount overflow")));

	let read_state = async || db.read(async |t| {
		let row = t.query_one("SELECT jsonb_build_object( \
			'vtxos',(SELECT jsonb_agg(to_jsonb(v) ORDER BY v.vtxo_id) FROM vtxo v), \
			'transactions',(SELECT jsonb_agg(to_jsonb(v) ORDER BY v.txid) FROM virtual_transaction v), \
			'htlcs',(SELECT jsonb_agg(to_jsonb(h) ORDER BY h.id) FROM htlc_vtxo h), \
			'vtxo_history',(SELECT COUNT(*) FROM vtxo_history), \
			'transaction_history',(SELECT COUNT(*) FROM virtual_transaction_history))::TEXT", &[]).await?;
		Ok(row.get::<_, String>(0))
	}).await.unwrap();
	let before = read_state().await;
	let mut client = srv.get_public_rpc().await;
	let err = client.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: vec![malformed.serialize()],
	}).await.expect_err("overflowing genesis must receive a typed refusal");
	assert_eq!(err.code(), tonic::Code::InvalidArgument);
	assert!(err.message().contains("onchain amount overflow"), "unexpected error: {err}");
	assert_eq!(read_state().await, before, "refused registration changed durable state");

	// The same connection can then provide only the genuine missing witnesses.
	client.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: vec![signed.serialize()],
	}).await.unwrap();
	let registered = db.read(async |t| t.get_user_vtxo_by_id(signed.id()).await).await.unwrap();
	assert_eq!(registered.spend_state, SpendState::Spendable);
	assert_eq!(registered.vtxo.serialize(), signed.serialize());
	for item in signed.transactions() {
		let tx = db.read(async |t| t.get_virtual_transaction_by_txid(item.tx.compute_txid()).await)
			.await.unwrap().expect("registered transaction");
		assert_eq!(tx.signed_tx(), Some(&item.tx));
	}
}

/// The sender cosigns an arkoor but never registers the signed chain, so the
/// server holds the checkpoint tx unsigned and the watchman can only wait when
/// the input's exit confirms. Once the owner took the input through its exit
/// clause, the checkpoint can never confirm, so registering the child must be
/// refused.
#[tokio::test]
async fn register_vtxo_transactions_refuses_child_of_exited_input() {
	let ctx = TestContext::new("server/register_vtxo_transactions_refuses_child_of_exited_input").await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool().watchmand().create().await;
	let wm = srv.watchmand();

	// bark1 refreshes a board into a round vtxo; the clone keeps a copy of it
	let bark1 = ctx.bark("bark1", &srv).funded(sat(500_000)).create().await;
	bark1.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	ctx.refresh_all(&srv, &[&bark1]).await;
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	bark1.sync().await;
	assert_eq!(bark1.vtxo_ids().await.len(), 1);
	let bark1_old = bark1.full_clone("bark1_old").await;

	// both wallets reach the server through the proxy, so neither of them
	// registers the arkoor output
	let proxy = srv.start_proxy_no_mailbox(DropRegistration).await;
	bark1.set_ark_url(&proxy.address).await;
	let bark2 = ctx.bark("bark2", &proxy.address).create().await;
	bark1.send_oor(bark2.address().await, sat(50_000)).await;
	let child_ids = bark2.vtxo_ids().await;
	assert_eq!(child_ids.len(), 1);
	let child = bark2.raw_vtxo(child_ids[0]).await;

	// the stale clone exits the input and claims it after the exit delta
	bark1_old.start_exit_all().await;
	complete_exit(&ctx, &bark1_old).await;
	bark1_old.claim_all_exits(bark1_old.get_onchain_address().await).await;
	let tip = ctx.generate_blocks(1).await;
	wm.wait_for_sync_height(tip).await;

	let mut client = srv.get_public_rpc().await;
	let err = client.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: vec![child.serialize()],
	}).await.expect_err("registering the child of an exited input must be refused");
	assert!(err.message().contains("exited onchain"), "unexpected error: {}", err.message());

	// the child stays unregistered
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let state = db.read(async |t| {
		let row = t.query_one(
			"SELECT spend_state::TEXT FROM vtxo WHERE vtxo_id = $1",
			&[&child_ids[0].to_string()],
		).await?;
		Ok(row.get::<_, String>(0))
	}).await.unwrap();
	assert_eq!(state, "unregistered");
}

/// Builds a bare-vtxo encoding that decodes back to a full vtxo with the same
/// id, the same point and policy, an empty history anchored to its own
/// terminal outpoint, and the given (forged) expiry. This works because the
/// Pubkey policy output does not commit to the outer expiry.
fn reanchored_vtxo(vtxo: &Vtxo<Full>, expiry: BlockHeight) -> Vtxo<Full> {
	let fake = Vtxo::<Bare, VtxoPolicy>::new(
		vtxo.point(),
		vtxo.policy().clone(),
		vtxo.amount(),
		expiry,
		vtxo.server_pubkey(),
		vtxo.exit_delta(),
		vtxo.point(),
	);
	<Vtxo<Full>>::deserialize(&fake.serialize())
		.expect("a bare encoding re-anchors as an empty-genesis full vtxo")
}


/// A registration may only add witnesses to a stored vtxo. Re-anchoring a
/// pubkey vtxo at its own point with an empty chain and a forged expiry
/// passes validation against the anchor, so the server has to compare it
/// with the stored vtxo and refuse it. Otherwise offboarding the forged vtxo
/// builds a connector that the watchman sweeps before the genuine expiry,
/// and the owner exits the original vtxo after being paid out.
#[tokio::test]
async fn register_vtxo_transactions_refuses_reanchoring_with_forged_expiry() {
	let ctx = TestContext::new(
		"server/register_vtxo_transactions_refuses_reanchoring_with_forged_expiry",
	).await;
	let srv = ctx.captaind("server").funded(btc(10)).create().await;
	let bark = ctx.bark("bark", &srv).funded(sat(1_000_000)).create().await;

	// board, then send to self so the vtxo has a signed arkoor chain
	bark.board_and_confirm_and_register(&ctx, sat(800_000)).await;
	bark.send_oor(&bark.address().await, sat(700_000)).await;
	let target_id = bark.vtxos().await.into_iter()
		.find(|v| v.vtxo.amount == sat(700_000))
		.expect("oor destination vtxo")
		.vtxo.id;

	// anyone can fetch the vtxo by id
	let mut client = srv.get_public_rpc().await;
	let blob = client.get_vtxo(protos::GetVtxoRequest {
		vtxo_id: target_id.to_bytes().to_vec(),
	}).await.unwrap().into_inner().vtxo;
	let vtxo = <Vtxo<Full>>::deserialize(&blob).unwrap();

	let forged = reanchored_vtxo(&vtxo, BlockHeight::new(0));
	assert_eq!(forged.id(), target_id);
	let err = client.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: vec![forged.serialize()],
	}).await.expect_err("re-anchoring a registered vtxo must be refused");
	assert!(err.message().contains("does not match the stored vtxo"),
		"unexpected error: {}", err.message());

	// the server still holds the genuine vtxo
	let blob = client.get_vtxo(protos::GetVtxoRequest {
		vtxo_id: target_id.to_bytes().to_vec(),
	}).await.unwrap().into_inner().vtxo;
	let stored = <Vtxo<Full>>::deserialize(&blob).unwrap();
	assert_eq!(stored.chain_anchor(), vtxo.chain_anchor());
	assert_eq!(stored.expiry_height(), vtxo.expiry_height());
	assert_eq!(stored.exit_depth(), vtxo.exit_depth());
}

/// Offboarding builds its connector from the stored input vtxos, and the
/// watchman sweeps the connector once the tip passes the connector expiry.
/// If a registration could forge the inputs' expiry to 0, the connector
/// would be swept long before the inputs expire, and the owner could then
/// exit the original vtxos after already being paid by the offboard.
#[tokio::test]
async fn reanchoring_cannot_make_watchman_sweep_offboard_connector_early() {
	let ctx = TestContext::new(
		"server/reanchoring_cannot_make_watchman_sweep_offboard_connector_early",
	).await;
	let srv = ctx.captaind("server").funded(btc(10)).no_vtxo_pool().watchmand().create().await;
	let wm = srv.watchmand();

	// four inputs make the connector root large enough to pay for its own sweep
	let bark = ctx.bark("bark", &srv).funded(sat(1_000_000)).create().await;
	for _ in 0..4 {
		bark.board_and_confirm_and_register(&ctx, sat(200_000)).await;
	}
	let ids = bark.vtxo_ids().await;
	assert_eq!(ids.len(), 4);

	let mut client = srv.get_public_rpc().await;
	let mut expiries = Vec::new();
	let mut forged = Vec::new();
	for id in &ids {
		let blob = client.get_vtxo(protos::GetVtxoRequest {
			vtxo_id: id.to_bytes().to_vec(),
		}).await.unwrap().into_inner().vtxo;
		let vtxo = <Vtxo<Full>>::deserialize(&blob).unwrap();
		expiries.push(vtxo.expiry_height());
		forged.push(reanchored_vtxo(&vtxo, BlockHeight::new(0)).serialize());
	}
	let min_expiry = *expiries.iter().min().unwrap();
	let max_expiry = *expiries.iter().max().unwrap();

	// Whether the server refuses this is checked above; this test checks
	// what an accepted forgery leads to.
	let _ = client.register_vtxo_transactions(protos::RegisterVtxoTransactionsRequest {
		vtxos: forged,
	}).await;

	let offboard_txid = bark.offboard_all(ctx.bitcoind().get_new_address()).await;
	ctx.generate_blocks(1).await;
	// the connector root is the second output of the offboard tx
	let connector_point = OutPoint::new(offboard_txid, 1);

	// Mine past what a forged connector expiry would be (0 + 144), but stay
	// before any of the inputs expire.
	let tip = ctx.generate_blocks(170).await;
	assert!(tip < min_expiry, "inputs must not have expired yet");
	wm.wait_for_sync_height(tip).await;
	wm.trigger_sweep().await;
	let tip = ctx.generate_blocks(1).await;
	wm.wait_for_sync_height(tip).await;

	let unspent = ctx.bitcoind().sync_client()
		.get_tx_out(&connector_point.txid, connector_point.vout, Some(true)).unwrap();
	assert!(unspent.is_some(),
		"watchman swept offboard connector {} at tip {} while the inputs only expire \
		at {}: the owner can exit the original vtxos and be paid twice",
		connector_point, tip, min_expiry,
	);

	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let connector_expiry = db.read(async |t| {
		let row = t.query_one(
			"SELECT expiry FROM vtxo WHERE vtxo_id = $1", &[&connector_point.to_string()],
		).await?;
		Ok(row.get::<_, i32>(0))
	}).await.unwrap();
	assert_eq!(connector_expiry as u32, max_expiry.to_u32() + 144);
}

/// A board output is cosigned and registered for one exit tx only, even
/// when a fee change makes the server build a different one.
#[tokio::test]
async fn board_output_is_bound_to_one_exit_tx() {
	let ctx = TestContext::new("server/board_output_is_bound_to_one_exit_tx").await;
	let srv = ctx.captaind("server").create().await;
	let info = srv.ark_info().await;
	let height = ctx.generate_blocks(1).await;
	srv.wait_for_sync_height(height).await;

	let user = Keypair::new(&ark::SECP, &mut thread_rng());
	let expiry = height + info.vtxo_lifetime;
	let amount = sat(100_000);
	let script = BoardBuilder::new(
		user.public_key(), expiry, info.server_pubkey, info.vtxo_exit_delta,
	).funding_script_pubkey();
	let address = Address::from_script(&script, Network::Regtest).unwrap();
	let txid = ctx.bitcoind().fund_addr(&address, amount).await;
	let funding = ctx.bitcoind().await_transaction(txid).await;
	srv.bitcoind().await_transaction(txid).await;
	let vout = funding.output.iter().position(|o| o.script_pubkey == script).unwrap();
	let utxo = OutPoint::new(txid, vout as u32);

	let owner_proof = super::test_owner_link(&srv, &user).await;
	let cosign = async |fee: Amount| {
		let builder = BoardBuilder::new(
			user.public_key(), expiry, info.server_pubkey, info.vtxo_exit_delta,
		).set_funding_details(amount, fee, utxo).unwrap().generate_user_nonces();
		let resp = srv.get_public_rpc().await.request_board_cosign(protos::BoardCosignRequest { retired_owner_proof: None,
			owner_proof: Some(owner_proof.into()),
			amount: amount.to_sat(),
			utxo: utxo.serialize(),
			expiry_height: expiry.to_u32(),
			user_pubkey: user.public_key().serialize().to_vec(),
			pub_nonce: builder.user_pub_nonce().serialize().to_vec(),
			funding_tx: bitcoin::consensus::serialize(&funding),
		}).await?.into_inner().try_into().unwrap();
		Ok::<_, tonic::Status>(builder.build_vtxo(&resp, &user).unwrap())
	};
	let first = cosign(Amount::ZERO).await.unwrap();

	srv.stop().await.unwrap();
	srv.config_mut().fees.board.base_fee = sat(1_000);
	srv.start().await.unwrap();

	let err = cosign(sat(1_000)).await.expect_err("a second exit tx for the same output");
	assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");
	assert!(err.message().contains("already cosigned"), "{err}");

	// An output cosigned before the binding existed can still be registered
	// with only one of its exits.
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let unbind = async || {
		db.write(async |t| Ok(t.execute("DELETE FROM board_exit", &[]).await?)).await.unwrap();
	};
	unbind().await;
	let second = cosign(sat(1_000)).await.unwrap();
	assert_ne!(first.id(), second.id());
	unbind().await;

	let height = ctx.generate_blocks(srv.config().required_board_confirmations as u32).await;
	srv.wait_for_sync_height(height).await;
	let mut rpc = srv.get_public_rpc().await;
	rpc.register_board_vtxo(protos::BoardVtxoRequest { board_vtxo: first.serialize() })
		.await.unwrap();
	let err = rpc.register_board_vtxo(protos::BoardVtxoRequest { board_vtxo: second.serialize() })
		.await.expect_err("a second exit tx for the same output");
	assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");
	assert!(err.message().contains("already registered"), "{err}");
}
