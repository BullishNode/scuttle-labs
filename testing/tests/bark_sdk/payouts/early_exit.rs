//! A wallet action that holds a coin the server pays out at expiry ends once
//! the wallet sees that payout.
//!
//! The proxy refuses the request each action is stuck on, and reports the
//! status a server gives once it decided the payout of a coin.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ark::lightning::{PaymentHash, Preimage};
use ark_testing::{btc, sat, TestContext};
use ark_testing::daemon::captaind::{ArkClient, Captaind};
use ark_testing::daemon::captaind::proxy::{ArkRpcProxy, ArkRpcProxyServer};
use bark::Wallet;
use bark::actions::lightning::pay;
use bark::actions::lightning::receive::{self, LightningReceiveState};
use bark::actions::offboard;
use bark::movement::{Movement, MovementStatus};
use bark::vtxo::{FallbackStatus, VtxoState};
use bitcoin::SignedAmount;
use bitcoin_ext::BlockDelta;
use cln_rpc::plugins::hold;
use server_rpc::protos;

#[derive(Clone, Default)]
struct PayoutProxy {
	/// Set once the server cosigned an arkoor or Lightning send.
	cosigned: Arc<AtomicBool>,
	/// Hold every registration after a cosign until the request is dropped.
	hold_registration: Arc<AtomicBool>,
	held: Arc<tokio::sync::Notify>,
	/// Refuse every registration after a cosign.
	refuse_registration: Arc<AtomicBool>,
	/// Refuse Lightning payment initiation, revocation and receive claims.
	refuse_lightning: Arc<AtomicBool>,
	/// Report every coin as paid out at expiry to `owner`.
	decide: Arc<AtomicBool>,
	owner: Arc<Mutex<Vec<u8>>>,
}

const PAYOUT: protos::FallbackPayout = protos::FallbackPayout {
	state: protos::FallbackState::Paid as i32, gross_sat: 10_000, fee_sat: 2_000, payout_sat: 8_000,
};

fn refusal() -> tonic::Status {
	tonic::Status::invalid_argument("refused by the test proxy")
}

#[async_trait::async_trait]
impl ArkRpcProxy for PayoutProxy {
	async fn request_arkoor_cosign(&self, upstream: &mut ArkClient, req: protos::ArkoorPackageCosignRequest)
		-> Result<protos::ArkoorPackageCosignResponse, tonic::Status>
	{
		let res = upstream.request_arkoor_cosign(req).await?.into_inner();
		self.cosigned.store(true, Ordering::SeqCst);
		Ok(res)
	}

	async fn request_lightning_pay_htlc_cosign(&self, upstream: &mut ArkClient, req: protos::LightningPayHtlcCosignRequest)
		-> Result<protos::ArkoorPackageCosignResponse, tonic::Status>
	{
		let res = upstream.request_lightning_pay_htlc_cosign(req).await?.into_inner();
		self.cosigned.store(true, Ordering::SeqCst);
		Ok(res)
	}

	async fn register_vtxo_transactions(&self, upstream: &mut ArkClient, req: protos::RegisterVtxoTransactionsRequest)
		-> Result<protos::Empty, tonic::Status>
	{
		if self.cosigned.load(Ordering::SeqCst) {
			if self.hold_registration.load(Ordering::SeqCst) {
				self.held.notify_one();
				std::future::pending::<()>().await;
			}
			if self.refuse_registration.load(Ordering::SeqCst) {
				return Err(refusal());
			}
		}
		Ok(upstream.register_vtxo_transactions(req).await?.into_inner())
	}

	async fn initiate_lightning_payment(&self, upstream: &mut ArkClient, req: protos::InitiateLightningPaymentRequest)
		-> Result<protos::Empty, tonic::Status>
	{
		if self.refuse_lightning.load(Ordering::SeqCst) {
			return Err(refusal());
		}
		Ok(upstream.initiate_lightning_payment(req).await?.into_inner())
	}

	async fn request_lightning_pay_htlc_revocation(&self, upstream: &mut ArkClient, req: protos::ArkoorPackageCosignRequest)
		-> Result<protos::ArkoorPackageCosignResponse, tonic::Status>
	{
		if self.refuse_lightning.load(Ordering::SeqCst) {
			return Err(refusal());
		}
		Ok(upstream.request_lightning_pay_htlc_revocation(req).await?.into_inner())
	}

	async fn claim_lightning_receive(&self, upstream: &mut ArkClient, req: protos::ClaimLightningReceiveRequest)
		-> Result<protos::ArkoorPackageCosignResponse, tonic::Status>
	{
		if self.refuse_lightning.load(Ordering::SeqCst) {
			return Err(refusal());
		}
		Ok(upstream.claim_lightning_receive(req).await?.into_inner())
	}

	async fn get_vtxo_status(&self, upstream: &mut ArkClient, req: protos::GetVtxoStatusRequest)
		-> Result<protos::GetVtxoStatusResponse, tonic::Status>
	{
		let mut res = upstream.get_vtxo_status(req).await?.into_inner();
		if self.decide.load(Ordering::SeqCst) {
			res.spend_state = protos::VtxoSpendState::Spent as i32;
			res.owner_mailbox = self.owner.lock().unwrap().clone();
			res.fallback_payout = Some(PAYOUT);
		}
		Ok(res)
	}

	async fn get_vtxos_status_by_ids(&self, upstream: &mut ArkClient, req: protos::GetVtxosStatusByIdsRequest)
		-> Result<protos::GetVtxosStatusByIdsResponse, tonic::Status>
	{
		let mut res = upstream.get_vtxos_status_by_ids(req).await?.into_inner();
		if self.decide.load(Ordering::SeqCst) {
			for status in &mut res.vtxos {
				status.spend_state = protos::VtxoSpendState::Spent as i32;
				status.owner_mailbox = self.owner.lock().unwrap().clone();
				status.fallback_payout = Some(PAYOUT);
			}
		}
		Ok(res)
	}
}

impl PayoutProxy {
	fn decide_for(&self, wallet: &Wallet) {
		*self.owner.lock().unwrap() = wallet.mailbox_identifier().as_pubkey().serialize().to_vec();
		self.decide.store(true, Ordering::SeqCst);
	}
}

async fn payout_server(ctx: &TestContext) -> (Arc<Captaind>, PayoutProxy, ArkRpcProxyServer) {
	let srv = ctx.captaind("server").no_vtxo_pool().cfg(|cfg| {
		cfg.expiry_payouts = true;
		cfg.vtxo_lifetime = BlockDelta::new(24);
	}).create().await;
	let proxy = PayoutProxy::default();
	let server = srv.start_proxy_no_mailbox(proxy.clone()).await;
	(srv, proxy, server)
}

async fn movement(wallet: &Wallet, subsystem: &str) -> Movement {
	let mut movements = wallet.history().await.unwrap().into_iter()
		.filter(|m| m.subsystem.name == subsystem).collect::<Vec<_>>();
	assert_eq!(movements.len(), 1, "{movements:?}");
	movements.remove(0)
}

async fn mine_past(ctx: &TestContext, srv: &Captaind, wallet: &Wallet, height: bitcoin_ext::BlockHeight) {
	let tip = ctx.bitcoind().get_block_count().await as u32;
	let tip = ctx.generate_blocks(height.to_u32() + 1 - tip).await;
	srv.wait_for_sync_height(tip).await;
	wallet.chain().invalidate_caches().await;
}

/// R1: an arkoor send stopped after the cosign whose outputs expired before
/// they were registered. The server refunds them to the sender, so the send
/// fails and nothing is delivered.
#[tokio::test]
async fn arkoor_send_with_unregistered_expired_outputs_fails() {
	let ctx = TestContext::new("bark_sdk/payouts/arkoor_send_unregistered_expired").await;
	let (srv, proxy, server) = payout_server(&ctx).await;
	let sender = ctx.bark_sdk("sender", &server).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let input = sender.spendable_vtxos().await.unwrap().remove(0);

	// The sender stops while it registers the cosigned outputs.
	proxy.hold_registration.store(true, Ordering::SeqCst);
	let address = recipient.new_address().await.unwrap();
	tokio::select! {
		res = sender.send_arkoor_payment(&address, sat(10_000)) => panic!("send finished: {res:?}"),
		_ = proxy.held.notified() => {},
	}
	assert_eq!(sender.pending_arkoor_sends().await.unwrap().len(), 1);

	// It comes back after the outputs expired; the server refuses them.
	mine_past(&ctx, &srv, &sender, input.expiry_height()).await;
	proxy.hold_registration.store(false, Ordering::SeqCst);
	proxy.refuse_registration.store(true, Ordering::SeqCst);
	sender.sync_pending_arkoor_sends().await.unwrap();

	assert!(sender.pending_arkoor_sends().await.unwrap().is_empty());
	assert_eq!(movement(&sender, "bark.arkoor").await.status, MovementStatus::Failed);
	assert_eq!(sender.get_vtxo_by_id(input.id()).await.unwrap().state, VtxoState::Spent);
	recipient.sync_mailbox().await.unwrap();
	assert!(recipient.history().await.unwrap().is_empty(), "the recipient was never paid");
}

/// R4: a Lightning send whose HTLCs the server refuses to revoke ends once
/// they are paid out at expiry.
#[tokio::test]
async fn lightning_send_ends_when_its_htlcs_are_paid_out() {
	let ctx = TestContext::new("bark_sdk/payouts/lightning_send_htlcs_paid_out").await;
	let lightning = ctx.new_lightning_setup("lightningd").await;
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let proxy = PayoutProxy::default();
	let server = srv.start_proxy_no_mailbox(proxy.clone()).await;
	let wallet = ctx.bark_sdk("bark", &server).boarded(btc(1))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	lightning.sync().await;

	let payment_hash = Preimage::random().compute_payment_hash();
	let invoice = lightning.external.hold_client().await.invoice(hold::InvoiceRequest {
		payment_hash: payment_hash.as_ref().to_vec(),
		amount_msat: 10_000_000,
		description: Some(hold::invoice_request::Description::Memo("payout".into())),
		min_final_cltv_expiry: Some(18),
		expiry: Some(3600),
		routing_hints: vec![],
	}).await.unwrap().into_inner().bolt11;

	// The server refuses to pay and then to revoke the HTLCs.
	proxy.refuse_lightning.store(true, Ordering::SeqCst);
	let _ = wallet.pay_lightning_invoice(invoice, None, false).await;
	let stuck = wallet.stuck_failed_lightning_sends().await.unwrap();
	assert_eq!(stuck.len(), 1);
	let pay::Progress::RevocationStuck { htlcs, .. } = &stuck[0].progress else {
		panic!("unexpected {:?}", stuck[0].progress);
	};
	let htlcs = htlcs.vtxo_ids.clone();

	// The HTLCs expired and were paid out to this wallet.
	proxy.decide_for(&wallet);
	let _ = wallet.check_lightning_payment(payment_hash, false).await;

	assert!(wallet.pending_lightning_sends().await.unwrap().is_empty());
	let send = movement(&wallet, "bark.lightning_send").await;
	assert_eq!((send.status, send.effective_balance), (MovementStatus::Failed, SignedAmount::ZERO));
	for id in htlcs {
		let htlc = wallet.get_vtxo_by_id(id).await.unwrap();
		assert_eq!((htlc.state, htlc.fallback_status), (VtxoState::Spent, Some(FallbackStatus::Spent)));
	}
	let balance = ark_testing::balance::assert_balance_consistent(&wallet, false).await;
	assert_eq!(balance.pending_lightning_send, bitcoin::Amount::ZERO);
}

/// R5: a Lightning receive whose claim the server refuses ends once its
/// grants are paid out at expiry, to this wallet or, for a forwarded
/// receive, to the recipient.
#[tokio::test]
async fn lightning_receive_ends_when_its_grants_are_paid_out() {
	let ctx = TestContext::new("bark_sdk/payouts/lightning_receive_grants_paid_out").await;
	let lightning = Arc::new(ctx.new_lightning_setup("lightningd").await);
	let srv = ctx.captaind("server").lightningd(&lightning.internal).funded(btc(10))
		.cfg(|cfg| cfg.expiry_payouts = true).create().await;
	srv.wait_for_vtxopool(&ctx).await;
	let proxy = PayoutProxy::default();
	let server = srv.start_proxy_no_mailbox(proxy.clone()).await;
	let wallet = ctx.bark_sdk("receiver", &server).cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv).create().await;
	lightning.sync().await;
	proxy.refuse_lightning.store(true, Ordering::SeqCst);

	for forwarded in [false, true] {
		proxy.decide.store(false, Ordering::SeqCst);
		let destination = if forwarded { Some(recipient.new_address().await.unwrap()) } else { None };
		let invoice = wallet.bolt11_invoice_builder().amount(sat(50_000)).claim_destination(destination)
			.build().await.unwrap();
		let hash = PaymentHash::from(&invoice);
		let (payer, bolt11) = (lightning.clone(), invoice.to_string());
		// The refused claim never settles the hold invoice.
		tokio::spawn(async move { payer.external.try_pay_bolt11(bolt11).await });
		srv.wait_for_lightning_receive_accepted(hash).await;

		// Refused claims leave the receive pending.
		let _ = wallet.try_claim_lightning_receive(hash, false).await;
		let LightningReceiveState::InProgress(receive) = wallet.lightning_receive_state(hash).await.unwrap() else {
			panic!("refused claim ended the receive");
		};
		let receive::Progress::PreimageRevealed(htlcs) = receive.progress else {
			panic!("unexpected {:?}", receive.progress);
		};

		// The grants are paid out: to this wallet, or to the recipient.
		if forwarded {
			*proxy.owner.lock().unwrap() = recipient.mailbox_identifier().as_pubkey().serialize().to_vec();
			proxy.decide.store(true, Ordering::SeqCst);
		} else {
			proxy.decide_for(&wallet);
		}
		let state = wallet.try_claim_lightning_receive(hash, false).await.unwrap();
		assert!(matches!(state, LightningReceiveState::Settled(_)), "{state:?}");
		let receive = wallet.history().await.unwrap().into_iter()
			.find(|m| m.id == htlcs.movement_id).unwrap();
		assert_eq!((receive.status, receive.effective_balance), (MovementStatus::Successful, SignedAmount::ZERO));
		for id in htlcs.vtxo_ids {
			let grant = wallet.get_vtxo_by_id(id).await.unwrap();
			let expected = if forwarded { None } else { Some(FallbackStatus::Spent) };
			assert_eq!((grant.state, grant.fallback_status), (VtxoState::Spent, expected));
		}
	}
}

/// R3: an offboard whose split outputs the server refuses to register ends
/// once they are paid out at expiry.
#[tokio::test]
async fn offboard_ends_when_its_coins_are_paid_out() {
	let ctx = TestContext::new("bark_sdk/payouts/offboard_coins_paid_out").await;
	let (_srv, proxy, server) = payout_server(&ctx).await;
	let wallet = ctx.bark_sdk("bark", &server).boarded(sat(800_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;

	proxy.refuse_registration.store(true, Ordering::SeqCst);
	let address = ctx.bitcoind().get_new_address();
	let _ = wallet.send_onchain(address, sat(100_000)).await;
	let pending = wallet.pending_offboards().await.unwrap();
	assert_eq!(pending.len(), 1);
	let offboard::Progress::ArkoorRegistrationRequired { offboard_vtxo_ids, change_vtxo_ids } = &pending[0].progress else {
		panic!("unexpected {:?}", pending[0].progress);
	};
	let coins = offboard_vtxo_ids.iter().chain(change_vtxo_ids).copied().collect::<Vec<_>>();

	proxy.decide_for(&wallet);
	wallet.sync_pending_offboards().await.unwrap();

	assert!(wallet.pending_offboards().await.unwrap().is_empty());
	let offboard = movement(&wallet, "bark.offboard").await;
	assert_eq!((offboard.status, offboard.effective_balance), (MovementStatus::Failed, SignedAmount::ZERO));
	for id in coins {
		assert_eq!(wallet.get_vtxo_by_id(id).await.unwrap().state, VtxoState::Spent);
	}
	assert_eq!(ark_testing::balance::assert_balance_consistent(&wallet, false).await.total(), bitcoin::Amount::ZERO);
}

/// R2: a delegated refresh is cancelled when the server pays out its input
/// at expiry.
#[tokio::test]
async fn delegated_refresh_is_cancelled_when_its_input_is_paid_out() {
	let ctx = TestContext::new("bark_sdk/payouts/delegated_refresh_input_paid_out").await;
	let (srv, proxy, server) = payout_server(&ctx).await;
	let wallet = ctx.bark_sdk("bark", &server).boarded(sat(100_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let input = wallet.spendable_vtxos().await.unwrap().remove(0);
	wallet.refresh_vtxos_delegated(vec![input.id()]).await.unwrap().expect("refresh joined");

	mine_past(&ctx, &srv, &wallet, input.expiry_height()).await;
	proxy.decide_for(&wallet);
	tokio::time::timeout(Duration::from_secs(60), wallet.sync()).await.unwrap();

	assert!(wallet.pending_round_states().await.unwrap().is_empty());
	let refresh = movement(&wallet, "bark.round").await;
	assert_eq!(refresh.status, MovementStatus::Failed);
	let paid = wallet.get_vtxo_by_id(input.id()).await.unwrap();
	assert_eq!((paid.state, paid.fallback_status), (VtxoState::Spent, Some(FallbackStatus::Spent)));
	let payout = wallet.fallback_payout(input.id()).await.unwrap().unwrap();
	assert_eq!((payout.gross, payout.fee, payout.payout), (sat(10_000), sat(2_000), sat(8_000)));
	assert_eq!(payout.address.script_pubkey(), wallet.fallback_script());
	assert_eq!(ark_testing::balance::assert_balance_consistent(&wallet, false).await.total(), bitcoin::Amount::ZERO);
}
