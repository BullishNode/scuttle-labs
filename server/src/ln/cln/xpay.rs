//! Manages outbound Lightning payments. Sends payments via CLN's `xpay` RPC
//! and tracks their status via `listsendpays` streams.
//!
//! ## Payment lifecycle
//!
//! [`ClnXpay::pay`] is fire-and-forget: it spawns a task that calls `xpay` over gRPC.
//! When CLN answers, success or error, the attempt is reconciled right away.
//! Its own Pending record keeps the attempt open even if xpay reported an error.
//! When the call breaks in transport, xpay may still be running, so the monitor
//! reconciles the attempt once its retry time has passed.
//!
//! ## Sendpay stream
//!
//! The main loop `wait`s on CLN for new `created` and `updated` sendpay events.
//! Each event is matched against open payment attempts in the DB and transitions
//! them through `Requested → Submitted → Succeeded/Failed`.
//!
//! ## Payment reconciliation
//!
//! On a periodic interval, queries `listpays` for all open attempts to catch anything
//! the stream missed (e.g. events during downtime). Uses exponential backoff per
//! invoice to avoid hammering CLN.

use std::{fmt, str};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bitcoin::{Amount, Network};
use bitcoin::secp256k1::PublicKey;
use bitcoin::hex::DisplayHex;
use chrono::{DateTime, Local};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, trace, warn};

use ark::lightning::{Invoice, PaymentHash, Preimage};
use bitcoin_ext::{AmountExt, BlockDelta, BlockHeight};
use cln_rpc::listpays_pays::ListpaysPaysStatus;

use crate::database;
use crate::database::ln::{LightningNodeId, LightningPaymentAttempt, LightningPaymentStatus};
use crate::database::xpay::{self, ClnXpayRecord, ClnXpayStatus};
use crate::ln::settler::HtlcSettler;
use crate::ln::guard::{PaymentGuard, PaymentGuards};
use crate::system::RuntimeManager;
use crate::telemetry;

use super::ClnGrpcClient;
use super::super::payment_handler::PaymentAttemptHandler;


/// The buffer we add to the xpay timeout before we check invoice
pub const XPAY_TIMEOUT_BUFFER: Duration = Duration::from_secs(15);

/// Shared client for sending xpay RPCs and reconciling payment status against CLN.
///
/// Wrapped in an `Arc` so both [`ClnXpay::pay`] (fire-and-forget spawned tasks)
/// and [`ClnXpayProcess`] (periodic reconciliation) can use it concurrently.
pub(crate) struct ClnXpayClient {
	db: database::Db,
	rpc: ClnGrpcClient,
	node_id: LightningNodeId,
	node_pubkey: PublicKey,
	network: Network,
	settler: Arc<HtlcSettler>,
	/// Notifies [`LightningManager::wait_payment_status`] when a payment reaches a final state.
	///
	/// [`LightningManager::wait_payment_status`]: crate::ln::node_manager::LightningManager::wait_payment_status
	payment_update_tx: broadcast::Sender<PaymentHash>,
	mailbox_manager: Arc<crate::mailbox_manager::MailboxManager>,
	payment_guards: PaymentGuards,
}

impl ClnXpayClient {
	fn payment_handler(&self) -> PaymentAttemptHandler<'_> {
		PaymentAttemptHandler::new(&self.db, &self.mailbox_manager, &self.payment_update_tx)
	}

	pub async fn new(
		db: database::Db,
		payment_update_tx: broadcast::Sender<PaymentHash>,
		rpc: ClnGrpcClient,
		node_id: LightningNodeId,
		node_pubkey: PublicKey,
		network: Network,
		settler: Arc<HtlcSettler>,
		mailbox_manager: Arc<crate::mailbox_manager::MailboxManager>,
		payment_guards: PaymentGuards,
	) -> Arc<Self> {
		Arc::new(Self { db, payment_update_tx, rpc, node_id, node_pubkey, network,
			settler, mailbox_manager, payment_guards })
	}

	pub(crate) async fn node_info(&self) -> anyhow::Result<cln_rpc::GetinfoResponse> {
		let info = super::unary_rpc(self.rpc.clone().getinfo(cln_rpc::GetinfoRequest {})).await?.into_inner();
		ensure!(qualified_node(&info, self.node_pubkey, self.network), "unsupported xpay node/version/network");
		Ok(info)
	}

	async fn admit_original_dispatch(
		&self, attempt: &LightningPaymentAttempt, request: &cln_rpc::XpayRequest,
		native_height: BlockHeight, payment_amount: Amount,
	) -> anyhow::Result<(ClnXpayRecord, u32)> {
		self.db.write(async |tx| {
			// Keep the same validity -> sources -> settlement -> attempt order as
			// terminal decisions. The process-local dispatch guard is held outside.
			tx.lock_settlement_sources(attempt.payment_hash).await?;
			let rows = tx.query("SELECT l.vtxo_id,h.payment_hash,h.direction::TEXT FROM lightning_payment_attempt_htlc_vtxo l \
				LEFT JOIN vtxo v ON v.vtxo_id=l.vtxo_id LEFT JOIN htlc_vtxo h ON h.id=v.id \
				WHERE l.lightning_payment_attempt_id=$1 ORDER BY l.vtxo_id", &[&attempt.id]).await?;
			ensure!(!rows.is_empty(), "xpay dispatch has no retained funding sources");
			let mut ids = Vec::with_capacity(rows.len());
			for row in rows {
				ensure!(row.get::<_, Option<&str>>("payment_hash") == Some(attempt.payment_hash.to_string().as_str())
					&& row.get::<_, Option<&str>>("direction") == Some("incoming"), "xpay funding source differs from its attempt");
				ids.push(row.get::<_, &str>("vtxo_id").parse()?);
			}
			crate::database::expiry_guard::guard_new_lightning_inputs(tx, &ids).await?;
			let tip = tx.get_highest_block(database::BlockTable::Captaind).await?
				.context("xpay dispatch has no processed server tip")?.height;
			for source in tx.get_user_vtxos_by_id(&ids).await? { source.check_htlc_send_spendable(tip)?; }
			tx.lock_htlc_settlements().await?;
			tx.ensure_not_settled(attempt.payment_hash).await?;
			ensure!(tx.query_opt("SELECT id FROM lightning_payment_attempt WHERE id=$1 \
				AND status IN ('requested','submitted') FOR UPDATE", &[&attempt.id]).await?.is_some(),
				"xpay attempt closed before dispatch admission");
			let current = tx.get_open_lightning_payment_attempt_by_payment_hash(attempt.payment_hash).await?
				.context("xpay attempt disappeared before dispatch admission")?;
			ensure!(current.id == attempt.id && current.lightning_node_id == self.node_id,
				"xpay dispatch no longer belongs to this attempt/node");
			let label = attempt.id.to_string();
			let record = xpay::get_xpays_by_payment_hash(&**tx, attempt.payment_hash).await?
				.into_iter().find(|record| record.label.as_deref() == Some(label.as_str()))
				.context("original xpay request disappeared before dispatch")?;
			ensure!(record.lightning_node_id == self.node_id && record.status == ClnXpayStatus::Pending
				&& record.error_code.is_none() && record.signed_invoice.as_deref() == Some(request.invstring.as_str())
				&& record.payment_amount_msat == payment_amount.to_msat(), "original xpay identity changed");
			let available = maxdelay_until(record.htlc_expiry.context("missing original HTLC bound")?,
				native_height, tip, current.block_height.context("missing attempt height")?)?;
			let maxdelay = request.maxdelay.context("missing original maxdelay")?.min(available.to_u32());
			ensure!(maxdelay > 0, "no original source delay remains before xpay dispatch");
			Ok((record, maxdelay))
		}).await
	}

	/// Calls xpay and then reconciles the attempt status against CLN.
	///
	/// On RPC success the sendpay stream will pick up the result, but we still
	/// reconcile afterwards to handle edge cases (e.g. the stream missing an event).
	/// A coded error is retained as context, not substituted for the native status.
	/// On a transport error reconciliation is left to the monitor.
	#[tracing::instrument(skip_all, fields(
		payment_hash = %invoice.payment_hash(),
		invoice = %invoice,
		payment_amount = %payment_amount,
		max_routing_fee = %max_routing_fee,
		max_cltv_expiry_delta,
		retry_for = ?retry_for,
	))]
	pub async fn pay(
		&self,
		attempt_id: i64,
		invoice: Box<Invoice>,
		payment_amount: Amount,
		max_routing_fee: Amount,
		max_cltv_expiry_delta: BlockDelta,
		retry_for: Duration,
	) {
		let payment_hash = invoice.payment_hash();
		let dispatch = self.payment_guards.lock_dispatch(payment_hash).await;
		let attempt = match self.db.read(async |t| t.get_open_lightning_payment_attempt_by_payment_hash(payment_hash).await).await {
			Ok(Some(current)) if current.id == attempt_id && current.lightning_node_id == self.node_id => current,
			Ok(_) => return,
			Err(error) => { error!(%payment_hash, %error, "Could not check queued payment"); return; },
		};
		let attempt_id = attempt.id;
		let payment_hash = attempt.payment_hash;
		let mut rpc = self.rpc.clone();
		let mut request = xpay_request(&invoice, payment_amount, max_routing_fee,
			max_cltv_expiry_delta, retry_for, attempt_id.to_string());
		let record = match async {
			let info = self.node_info().await?;
			self.admit_original_dispatch(&attempt, &request, BlockHeight::new(info.blockheight), payment_amount).await
		}.await {
			Ok((record, maxdelay)) => { request.maxdelay = Some(maxdelay); record },
			Err(error) => {
				error!(attempt_id, %payment_hash, %error, "Native dispatch was not admitted; retaining custody");
				return;
			},
		};
		// Only place CLN says whether the route was pruned on cost or on delay.
		// A route it never found leaves no `listpays` record either, so
		// dropping this loses the reason entirely.
		let result = call_xpay(
			&mut rpc, &invoice, payment_amount, max_routing_fee, &request,
		).await;
		// Only this original live reply can provide zero-part evidence. A
		// stored error or empty monitor lookup cannot reconstruct it.
		let zero_part = result.as_ref().err().and_then(native_xpay_error_code)
			.filter(|code| matches!(code, 205 | 206 | 207));
		let zero_part = if zero_part.is_some() && self.node_info().await.is_ok() { zero_part } else { None };
		let pay_error = match result {
			Ok(_preimage) => {
				// NB we don't do db stuff when it's succesful, because
				// it will happen in the sendpay stream of the monitor process
				trace!("Payment successful for payment hash {}", payment_hash.as_hex());
				None
			},
			Err(pay_err) if !is_cln_error(&pay_err) => {
				// xpay may still be trying, and syncing now could fail an
				// attempt CLN goes on to pay.
				warn!("xpay call for {} broke, leaving it to the monitor: {:#}",
					payment_hash, pay_err,
				);
				return;
			},
			Err(pay_err) => {
				debug!("Error calling pay-command: {}", pay_err);
				Some(pay_err.to_string())
			},
		};

		// A later attempt for this hash, maybe on another node, is not ours to
		// reconcile with this node's xpay result.
		let attempt_res = self.db
			.read(async |t| t.get_open_lightning_payment_attempt_by_payment_hash(payment_hash).await).await
			.map(|a| a.filter(|a| a.id == attempt_id));

		let attempt_res = match (attempt_res, pay_error.as_deref()) {
			// Store the error before reconciling, so the reason survives if
			// `listpays` fails and the monitor has to finish the attempt.
			(Ok(Some(attempt)), Some(e)) => self.store_pay_error(attempt, e).await,
			(res, _) => res,
		};

		match attempt_res {
			Ok(Some(attempt)) => {
				if let Err(e) = self.reconcile_payment_attempt_locked(
					attempt, pay_error.as_deref(), zero_part.map(|code| (&record, code)), false, dispatch,
				).await {
					error!("Error syncing payment attempt status: {e:#}");
				}
			},
			Ok(None) => {
				error!("Attempt {} for payment hash {} was closed before xpay returned",
					attempt_id, payment_hash,
				);
			},
			Err(e) => {
				error!("Error getting open payment attempt by payment hash: {e:#}");
			},
		}
	}

	/// Stores the xpay error on `attempt` and returns it as stored, so later
	/// updates carry the new `updated_at`. When the attempt was changed in the
	/// meantime, it is reloaded and the store retried. Returns `None` once the
	/// attempt is no longer open, leaving a finalized attempt untouched.
	async fn store_pay_error(
		&self,
		mut attempt: LightningPaymentAttempt,
		error: &str,
	) -> anyhow::Result<Option<LightningPaymentAttempt>> {
		loop {
			let updated_at = self.db.write(async |t|
				t.set_lightning_payment_attempt_error(&attempt, error).await
			).await?;
			if let Some(updated_at) = updated_at {
				attempt.updated_at = updated_at;
				attempt.error = Some(error.to_owned());
				return Ok(Some(attempt));
			}

			let reloaded = self.db.read(async |t|
				t.get_open_lightning_payment_attempt_by_payment_hash(attempt.payment_hash).await
			).await?;
			match reloaded {
				Some(a) if a.id == attempt.id => attempt = a,
				_ => return Ok(None),
			}
		}
	}

	/// Queries CLN's `listpays` for the given attempt and updates the DB to match.
	///
	/// If CLN has no own record, keep the attempt open for observation only.
	/// Neither an error nor elapsed height proves a native terminal outcome.
	/// If CLN reports a different status than the DB, the DB is
	/// updated (unless the DB status is already final, which is logged as an error).
	/// Sends on `payment_update_tx` when the status changes.
	///
	/// The monitor has no original live reply to supply failure evidence.
	async fn sync_payment_attempt_status(
		&self, attempt: LightningPaymentAttempt,
	) -> anyhow::Result<()> {
		let payment_hash = attempt.payment_hash;
		ensure!(attempt.lightning_node_id == self.node_id, "attempt belongs to a different native node");
		let Some(dispatch) = self.payment_guards.try_lock_dispatch(payment_hash) else { return Ok(()); };
		self.reconcile_payment_attempt_locked(attempt, None, None, false, dispatch).await
	}

	/// Observe existing work. Only privileged external confirmation of complete
	/// same-backend cold cessation permits resolving an otherwise empty orphan.
	pub async fn reconcile_existing_payment_attempt(
		&self, attempt: LightningPaymentAttempt, native_cold_restart_confirmed: bool,
	) -> anyhow::Result<()> {
		ensure!(!attempt.is_self_payment(), "intra-Ark attempts resolve through their receive");
		let dispatch = self.payment_guards.try_lock_dispatch(attempt.payment_hash)
			.context("original dispatch is still active; attempt stays held")?;
		self.node_info().await?;
		self.reconcile_payment_attempt_locked(attempt, None, None, native_cold_restart_confirmed, dispatch).await
	}

	/// Consume a live result without releasing its dispatch lock to the monitor.
	async fn reconcile_payment_attempt_locked(
		&self, attempt: LightningPaymentAttempt, pay_error: Option<&str>,
		zero_part: Option<(&ClnXpayRecord, i32)>, native_cold_restart_confirmed: bool, dispatch: PaymentGuard,
	) -> anyhow::Result<()> {
		let payment_hash = attempt.payment_hash;
		ensure!(dispatch.payment_hash() == payment_hash, "dispatch guard belongs to a different payment hash");
		ensure!(attempt.lightning_node_id == self.node_id, "attempt belongs to a different native node");
		let error = pay_error.or(attempt.error.as_deref());
		debug!("Lightning payment attempt ({}): with payment hash {} is being verified.",
			attempt.id, payment_hash,
		);

		telemetry::add_payment_sync(attempt.lightning_node_id, attempt.status);

		let req = cln_rpc::ListpaysRequest {
			bolt11: None,
			payment_hash: Some(payment_hash.to_vec()),
			status: None,
			index: None,
			limit: None,
			start: None,
		};
		let mut listpays_response = super::unary_rpc(self.rpc.clone().list_pays(req)).await
			.context("Could not fetch cln payments")?
			.into_inner();
		let label = attempt.id.to_string();
		let hash_pending_or_complete = retain_own_pays(&mut listpays_response.pays, payment_hash, &label)?;
		if listpays_response.pays.is_empty() {
			if let Some((record, code)) = zero_part.filter(|_| !hash_pending_or_complete) {
				self.payment_handler().process_native_failure(&self.settler, &attempt, error, record, Some(code)).await?;
				return Ok(());
			}
			if native_cold_restart_confirmed {
				ensure!(!hash_pending_or_complete, "same-hash native work is active or paid; orphan stays held");
				let parts = super::unary_rpc(self.rpc.clone().list_send_pays(cln_rpc::ListsendpaysRequest {
					payment_hash: Some(payment_hash.to_vec()), ..Default::default()
				})).await?.into_inner().payments;
				for part in parts {
					ensure!(part.payment_hash == payment_hash.to_vec()
						&& part.payment_preimage.is_none()
						&& matches!(cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::try_from(part.status),
							Ok(cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Failed)),
						"same-hash native parts are paid, pending or unknown; orphan stays held");
				}
				self.node_info().await?;
				let record = self.db.read(async |tx| Ok(xpay::get_xpays_by_payment_hash(&**tx, payment_hash).await?
					.into_iter().find(|record| record.label.as_deref() == Some(label.as_str())))).await?
					.context("orphan has no original native request")?;
				self.payment_handler().process_native_failure(&self.settler, &attempt,
					Some("operator confirmed same-backend cold restart; no active or successful native payment"),
					&record, None).await?;
				return Ok(());
			}
			match attempt.status {
				LightningPaymentStatus::Succeeded => {
					error!("Lightning payment attempt ({}): flagged succeeded \
						when it cannot be found in CLN for payment hash {}",
						attempt.id, payment_hash,
					);
				},
				LightningPaymentStatus::Failed => {
					error!("Lightning payment attempt ({}): flagged failed \
						when it cannot be found in CLN for payment hash {}",
						attempt.id, payment_hash,
					)
				},
				LightningPaymentStatus::Requested
					| LightningPaymentStatus::Submitted =>
				{
					error!(attempt_id = attempt.id, %payment_hash, status = %attempt.status,
						pay_error_present = error.is_some(), hash_pending_or_complete,
						"Own native payment lookup is empty; retaining custody without resubmitting");
				},
			}
		} else {
			// A newer failed group cannot override another own Pending group
			// or an actual successful payment. Unknown enum values also hold.
			let latest = select_own_payment(listpays_response.pays)?;

			let updated_status = match latest.status() {
				ListpaysPaysStatus::Pending => LightningPaymentStatus::Submitted,
				ListpaysPaysStatus::Complete => {
					if latest.preimage.is_none() {
						error!("Lightning payment attempt ({}): completed but no preimage \
							specified for payment hash {}",
							attempt.id, payment_hash,
						);
						LightningPaymentStatus::Submitted
					} else {
						LightningPaymentStatus::Succeeded
					}
				},
				ListpaysPaysStatus::Failed => LightningPaymentStatus::Failed,
			};

			// The onion is binary, so keep it as hex next to the xpay error.
			let onion = latest.erroronion.as_ref().map(|b| format!("erroronion={}", b.as_hex()));
			let error_string = match (error, onion) {
				(Some(e), Some(o)) => Some(format!("{e}; {o}")),
				(e, o) => e.map(String::from).or(o),
			};

			if attempt.status != updated_status {
				if attempt.status.is_final() {
					error!("Lightning payment attempt ({}): flagged {} when it \
						actually {} for payment hash {}",
						attempt.id, attempt.status, updated_status,
						payment_hash,
					);
				} else {
					let preimage = latest.preimage.map(|b| Preimage::from_slice(&b))
						.transpose()
						.context("CLN returned a preimage that is not 32 bytes")?;

					if let Some(preimage) = &preimage {
						if preimage.compute_payment_hash() != attempt.payment_hash {
							bail!("preimage does not match payment hash");
						}
					}

					// NB: for intra-ark payments, settle_invoice may also post
					// the mailbox notification for the same payment hash.
					self.payment_handler().process_payment_attempt(
						&self.settler,
						&attempt,
						updated_status,
						error_string.as_deref(),
						latest.amount_sent_msat.map(|v| v.msat),
						preimage,
					).await?;
				}
			}
		}

		Ok(())
	}
}

/// Timing knobs for the xpay monitor loop and reconciliation backoff.
#[derive(Debug, Clone)]
pub struct ClnXpayConfig {
	pub invoice_check_interval: Duration,
	/// Retry time for attempts that were stored without one.
	pub cln_xpay_timeout: Duration,
	pub check_base_delay: Duration,
	pub max_check_delay: Duration,
}

/// Handle for the xpay monitor process.
///
/// Tracks outbound payment status via CLN's listsendpays streams
/// and periodically verifies open payment attempts.
pub struct ClnXpay {
	jh: Option<JoinHandle<anyhow::Result<()>>>,
	client: Arc<ClnXpayClient>,
}

impl ClnXpay {
	/// Spawns the background reconciliation loop and returns a handle.
	///
	/// Reads the current payment stream indices from the DB so the monitor
	/// knows where to resume after a restart.
	pub async fn start(
		rtmgr: RuntimeManager,
		mgr_waker: Arc<Notify>,
		db: database::Db,
		payment_update_tx: broadcast::Sender<PaymentHash>,
		node_id: LightningNodeId,
		node_pubkey: PublicKey,
		network: Network,
		rpc: ClnGrpcClient,
		config: ClnXpayConfig,
		settler: Arc<HtlcSettler>,
		mailbox_manager: Arc<crate::mailbox_manager::MailboxManager>,
		payment_guards: PaymentGuards,
	) -> anyhow::Result<ClnXpay> {
		let payment_idxs = db.read(async |t| t.get_lightning_payment_indexes(node_id).await).await
			.with_context(|| format!("failed to fetch payment indices for {}", node_id))?
			.unwrap_or_default();

		slog!(XpayStarted,
			node_id: node_id,
			created_index: payment_idxs.created_index,
			updated_index: payment_idxs.updated_index,
		);

		let client = ClnXpayClient::new(db.clone(), payment_update_tx, rpc, node_id, node_pubkey,
			network, settler, mailbox_manager, payment_guards).await;

		let proc = ClnXpayProcess {
			config, db, node_id,
			client: client.clone(),
			attempt_next_check_at: HashMap::new(),
		};

		let jh = tokio::spawn(async move {
			let ret = proc.run(rtmgr, mgr_waker).await;
			if let Err(ref e) = ret {
				slog!(XpayStopped, node_id: node_id, error: format!("{:?}", e));
			}
			ret
		});

		Ok(ClnXpay { jh: Some(jh), client })
	}

	pub fn is_running(&self) -> bool {
		self.jh.as_ref().is_some_and(|jh| !jh.is_finished())
	}

	/// Cheap clone of the shared command client. Used by [`NodeHandle`] to
	/// issue xpay payments directly without going through the spawn helper.
	pub(crate) fn client(&self) -> Arc<ClnXpayClient> {
		self.client.clone()
	}

	/// Wait for the process to end.
	pub async fn wait(mut self) -> Result<anyhow::Result<()>, tokio::task::JoinError> {
		match self.jh.take() {
			Some(jh) => Ok(jh.await?),
			None => Ok(Ok(())),
		}
	}

}

impl Drop for ClnXpay {
	fn drop(&mut self) {
		if let Some(jh) = self.jh.take() {
			jh.abort();
		}
	}
}

impl fmt::Debug for ClnXpay {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ClnXpay")
	}
}

/// Background loop that periodically reconciles open payment attempts against CLN.
///
/// Runs on a fixed interval ([`ClnXpayConfig::invoice_check_interval`]) and,
/// for each open attempt old enough to have finished xpay retries, queries
/// `listpays` to drive the attempt to a final state. Uses exponential backoff
/// per invoice to avoid hammering CLN for long-lived pending payments.
struct ClnXpayProcess {
	config: ClnXpayConfig,
	db: database::Db,

	node_id: LightningNodeId,

	client: Arc<ClnXpayClient>,

	/// Per-attempt backoff state: maps attempt id → (check count, earliest next check time).
	/// Entries are pruned once their next-check time lapses.
	attempt_next_check_at: HashMap<i64, (usize, DateTime<Local>)>,
}

impl ClnXpayProcess {
	/// Bumps the backoff counter for `attempt_id` and schedules the next check.
	///
	/// Delay doubles each check starting from `check_base_delay`, capped at
	/// `max_check_delay`.
	fn update_next_attempt_check(&mut self, attempt_id: i64) {
		let (checks, next_check) = self.attempt_next_check_at.entry(attempt_id)
			.or_insert((0, Local::now()));
		*checks += 1;

		// Calculate delay: grows with each check
		// e.g. base 10 seconds, doubling each time, capped to a max delay
		let base_delay_secs = self.config.check_base_delay.as_secs();
		let max_delay_secs = self.config.max_check_delay.as_secs();
		let delay_secs = (base_delay_secs.saturating_mul(2u64.saturating_pow(*checks as u32 - 1)))
			.min(max_delay_secs);

		*next_check = Local::now() + Duration::from_secs(delay_secs);

		trace!("Lightning payment attempt ({}): Check {} done, updated next check to {}.",
			attempt_id, checks, next_check,
		);
	}

	/// Iterates over all open payment attempts for this node and reconciles
	/// each one that is old enough and not in backoff. Prunes expired backoff
	/// entries afterwards.
	async fn process_payment_attempts(&mut self) -> anyhow::Result<()> {
		let open_attempts = self.db.read(async |t| t.get_open_lightning_payment_attempts(self.node_id).await).await?;

		for attempt in open_attempts {
			if attempt.is_self_payment() {
				trace!("Lightning payment attempt ({}): Skipping since it is a self payment.",
					attempt.id,
				);

				continue;
			}

			// Initial polling delay only, followed by the existing per-attempt
			// backoff. Neither this age nor an empty lookup proves native routing
			// ended. Legacy fallback here is pacing, never authority to resend.
			let retry_for = attempt.retry_for.unwrap_or(self.config.cln_xpay_timeout);
			let safe_delay_cln_stopped_retries = retry_for + XPAY_TIMEOUT_BUFFER;
			if attempt.created_at > Local::now() - safe_delay_cln_stopped_retries {
				trace!("Lightning payment attempt ({}): Skipping since it was just created.",
					attempt.id,
				);

				continue;
			}

			let next_check = self.attempt_next_check_at.get(&attempt.id);
			if next_check.is_some() && next_check.unwrap().1 > Local::now() {
				trace!("Lightning payment attempt ({}): Skipping since it was checked recently.",
					attempt.id,
				);

				continue;
			}

			let attempt_id = attempt.id;
			if let Err(e) = self.client.sync_payment_attempt_status(attempt).await {
				error!("Error syncing payment attempt status: {e:#}");
			} else {
				self.update_next_attempt_check(attempt_id);
			}

		}

		self.attempt_next_check_at.retain(|_, &mut (_, datetime)| datetime > Local::now());

		telemetry::set_pending_payment_syncs(
			self.node_id,
			self.attempt_next_check_at.len(),
		);

		Ok(())
	}

	/// Main select loop: runs [`process_payment_attempts`](Self::process_payment_attempts)
	/// on every tick and exits cleanly on shutdown.
	async fn run(mut self, rtmgr: RuntimeManager, mgr_waker: Arc<Notify>) -> anyhow::Result<()> {
		let _worker = rtmgr.spawn(format!("ClnXpay({})", self.node_id))
			.with_notify(mgr_waker);

		let mut check_interval = tokio::time::interval(self.config.invoice_check_interval);

		loop {
			tokio::select! {
				_ = check_interval.tick() => {
					self.process_payment_attempts().await?;
				},
				_ = rtmgr.shutdown_signal() => return Ok(()),
			}
		}
	}
}

/// Bound a relative native request by its original absolute source deadline.
/// Neither CLN lag nor time spent waiting to dispatch can enlarge that budget.
fn maxdelay_until(
	deadline: BlockHeight, cln_tip: BlockHeight, server_tip: BlockHeight, attempt_height: BlockHeight,
) -> anyhow::Result<BlockDelta> {
	let tip = cln_tip.max(server_tip).max(attempt_height);
	let blocks = deadline.checked_blocks_since(tip).context("source deadline passed before xpay dispatch")?;
	ensure!(blocks > 0, "no source delay remains before xpay dispatch");
	Ok(BlockDelta::try_from(blocks).unwrap_or(BlockDelta::MAX))
}

fn qualified_node(info: &cln_rpc::GetinfoResponse, pubkey: PublicKey, network: Network) -> bool {
	info.id.as_slice() == pubkey.serialize().as_slice() && info.network == network.to_string()
		&& info.version == "v26.06.6"
}





/// Retain own status records, but do not hide a same-hash active or paid group
/// when an original zero-part call asks to retire its source.
fn retain_own_pays(pays: &mut Vec<cln_rpc::ListpaysPays>, hash: PaymentHash, label: &str) -> anyhow::Result<bool> {
	ensure!(pays.iter().all(|pay| pay.payment_hash == hash.to_vec()), "native payment lookup returned a different hash");
	let mut pending_or_complete = false;
	for pay in pays.iter() {
		let status = ListpaysPaysStatus::try_from(pay.status).context("unknown native payment status")?;
		pending_or_complete |= matches!(status, ListpaysPaysStatus::Pending | ListpaysPaysStatus::Complete);
	}
	pays.retain(|pay| pay.label.as_deref() == Some(label));
	Ok(pending_or_complete)
}

/// cln-grpc 26.06.6 formats the *outer* Xpay RpcError using Debug. Only the
/// complete exact envelope with data=None is supported here; a future encoding,
/// transport/deadline status, embedded code text or another method fails closed.
fn native_xpay_error_code(error: &anyhow::Error) -> Option<i32> {
	let status = error.downcast_ref::<tonic::Status>()?;
	if status.code() != tonic::Code::Unknown { return None; }
	let rest = status.message().strip_prefix("Error calling method Xpay: RpcError { code: Some(")?;
	let (code, rest) = rest.split_once("), message: ")?;
	let parsed: i32 = code.parse().ok()?;
	if code != parsed.to_string() { return None; }
	let message = rest.strip_suffix(", data: None }")?;
	serde_json::from_str::<String>(message).ok()?;
	Some(parsed)
}

fn select_own_payment(payments: Vec<cln_rpc::ListpaysPays>) -> anyhow::Result<cln_rpc::ListpaysPays> {
	for payment in &payments {
		ensure!(ListpaysPaysStatus::try_from(payment.status).is_ok(), "unknown native payment status");
		ensure!(payment.created_index.is_some(), "native payment has no created index");
	}
	payments.into_iter().max_by_key(|p| {
		let priority = match p.status() {
			ListpaysPaysStatus::Complete => 2,
			ListpaysPaysStatus::Pending => 1,
			ListpaysPaysStatus::Failed => 0,
		};
		(priority, p.created_index)
	}).context("no own native payment")
}

/// Whether lightningd itself answered with a coded RPC error. This is not a
/// terminal payment record. cln-grpc words every failed call as `Error calling
/// method ...`, but only lightningd carries `RpcError { code: Some(..) }`.
fn is_cln_error(err: &anyhow::Error) -> bool {
	err.downcast_ref::<tonic::Status>()
		.is_some_and(|s| s.message().contains("RpcError { code: Some("))
}

fn xpay_request(
	invoice: &Invoice, payment_amount: Amount, max_routing_fee: Amount,
	max_cltv_expiry_delta: BlockDelta, retry_for: Duration, label: String,
) -> cln_rpc::XpayRequest {
	cln_rpc::XpayRequest {
		invstring: invoice.to_string(),
		// cln doesn't allow tipping
		amount_msat: if invoice.amount_msat().is_none() {
			Some(payment_amount.into())
		} else {
			None
		},
		maxdelay: Some(max_cltv_expiry_delta.to_u32()),
		maxfee: Some(max_routing_fee.into()),
		retry_for: Some(retry_for.as_secs() as u32),
		partial_msat: None,
		layers: vec![],
		payer_note: None,
		label: Some(label),
		localinvreqid: None,
		dev_use_shadow: None,
	}
}

/// Calls the xpay-command over gRPC.
/// If the payment completes successfully it will return the pre-image
/// Otherwise, an error will be returned
async fn call_xpay(
	rpc: &mut ClnGrpcClient,
	invoice: &Invoice,
	payment_amount: Amount,
	max_routing_fee: Amount,
	request: &cln_rpc::XpayRequest,
) -> anyhow::Result<Preimage> {
	let payment_hash = invoice.payment_hash();

	slog!(XpayRpcCalled,
		payment_hash, payment_amount, max_routing_fee,
		invoice: invoice.to_string(),
		max_delay: request.maxdelay.context("xpay request has no maxdelay")?,
	);

	// The unary deadline releases this waiter, not the native payment. CLN
	// may retain its parts and complete after this future/HTTP2 stream ends.
	let deadline = Duration::from_secs(request.retry_for.context("xpay request has no retry time")?.into())
		+ XPAY_TIMEOUT_BUFFER;
	let mut rpc_request = tonic::Request::new(request.clone());
	rpc_request.set_timeout(deadline);
	let pay_result = match tokio::time::timeout(deadline, rpc.xpay(rpc_request)).await {
		Ok(result) => result,
		Err(_) => Err(tonic::Status::deadline_exceeded("xpay RPC deadline elapsed; native payment remains unresolved")),
	};

	let result = match pay_result {
		Err(e) => Err(e.into()),
		Ok(resp) => {
			let bytes = resp.into_inner().payment_preimage;
			if bytes.is_empty() {
				Err(anyhow!("missing preimage"))
			} else {
				bytes.try_into().ok().context("invalid preimage not 32 bytes")
			}
		}
	};

	slog!(XpayRpcReturned,
		payment_hash: payment_hash,
		error: result.as_ref().err().map(|e| e.to_string()),
	);

	result
}

#[cfg(test)]
mod test {
	use super::*;



	#[test]
	fn maxdelay_is_bounded_by_every_observed_or_recorded_height() {
		let height = BlockHeight::new;
		assert_eq!(maxdelay_until(height(200), height(90), height(120), height(110)).unwrap(), BlockDelta::new(80));
		assert_eq!(maxdelay_until(height(200), height(130), height(120), height(110)).unwrap(), BlockDelta::new(70));
		assert_eq!(maxdelay_until(height(200), height(90), height(120), height(140)).unwrap(), BlockDelta::new(60));
		assert!(maxdelay_until(height(200), height(90), height(201), height(110)).is_err());
		assert!(maxdelay_until(height(200), height(200), height(120), height(110)).is_err());
		assert!(maxdelay_until(height(200), height(90), height(200), height(110)).is_err());
		assert!(maxdelay_until(height(200), height(90), height(120), height(200)).is_err());
		assert_eq!(maxdelay_until(height(200), height(199), height(120), height(110)).unwrap(), BlockDelta::new(1));
		assert_eq!(maxdelay_until(height(200), height(90), height(199), height(110)).unwrap(), BlockDelta::new(1));
		assert_eq!(maxdelay_until(height(200), height(90), height(120), height(199)).unwrap(), BlockDelta::new(1));
	}

	#[test]
	fn is_cln_error_tells_cln_errors_from_transport_errors() {
		let cln = tonic::Status::unknown("Error calling method Xpay: RpcError { \
			code: Some(205), message: \"no route\", data: None }");
		assert!(is_cln_error(&cln.into()));

		let transport = tonic::Status::unavailable("transport error");
		assert!(!is_cln_error(&transport.into()));
		let reset = tonic::Status::unknown("h2 protocol error: stream reset");
		assert!(!is_cln_error(&reset.into()));
		let socket = tonic::Status::unknown("Error calling method Xpay: RpcError { \
			code: None, message: \"no response from lightningd\", data: None }");
		assert!(!is_cln_error(&socket.into()));
		assert!(!is_cln_error(&anyhow!("missing preimage")));
	}
	#[test]
	fn native_no_route_requires_the_exact_outer_reply() {
		let reply = "Error calling method Xpay: RpcError { code: Some(205), message: \"no route\", data: None }";
		assert_eq!(native_xpay_error_code(&tonic::Status::unknown(reply).into()), Some(205));
		assert_eq!(native_xpay_error_code(&tonic::Status::unknown(reply.replace("Some(205)", "Some(-4)")).into()), Some(-4));
		for error in [
			tonic::Status::deadline_exceeded(reply),
			tonic::Status::unavailable(reply),
			tonic::Status::unknown(reply.replace("method Xpay", "method Pay")),
			tonic::Status::unknown(format!("prefix {reply}")),
			tonic::Status::unknown(format!("{reply} trailing")),
			tonic::Status::unknown(reply.replace("Some(205)", "Some(+205)")),
			tonic::Status::unknown(reply.replace("data: None", "data: Some(Object {})")),
			tonic::Status::unknown("Error calling method Xpay: RpcError { code: None, message: \"code: Some(205)\", data: None }"),
			tonic::Status::unknown("Error calling method Xpay: RpcError { code: Some(-4), message: \"RpcError { code: Some(205), message: no route }\", data: None }"),
		] {
			assert_ne!(native_xpay_error_code(&error.into()), Some(205));
		}
		assert_eq!(native_xpay_error_code(&anyhow!(reply)), None);
	}





	#[test]
	fn native_own_pending_and_success_precede_another_failed_group() {
		let failed = cln_rpc::ListpaysPays {
			status: ListpaysPaysStatus::Failed as i32, created_index: Some(3), ..Default::default()
		};
		let pending = cln_rpc::ListpaysPays {
			status: ListpaysPaysStatus::Pending as i32, created_index: Some(1), ..Default::default()
		};
		let complete = cln_rpc::ListpaysPays {
			status: ListpaysPaysStatus::Complete as i32, created_index: Some(2),
			preimage: Some(vec![1; 32]), ..Default::default()
		};
		assert_eq!(select_own_payment(vec![pending.clone(), failed.clone()]).unwrap().status(), ListpaysPaysStatus::Pending);
		assert_eq!(select_own_payment(vec![pending, failed, complete]).unwrap().status(), ListpaysPaysStatus::Complete);
		assert!(select_own_payment(vec![]).is_err()); // empty is not a failed native record
		assert!(select_own_payment(vec![cln_rpc::ListpaysPays { status: 99, ..Default::default() }]).is_err());
		assert!(select_own_payment(vec![cln_rpc::ListpaysPays {
			status: ListpaysPaysStatus::Failed as i32, ..Default::default()
		}]).is_err());
		let hash = Preimage::random().compute_payment_hash();
		let mut foreign = vec![cln_rpc::ListpaysPays { label: Some("another-attempt".into()),
			payment_hash: hash.to_vec(), status: ListpaysPaysStatus::Complete as i32,
			preimage: Some(vec![1; 32]), created_index: Some(1), ..Default::default() }];
		assert!(retain_own_pays(&mut foreign, hash, "42").unwrap(), "foreign success blocks a zero-part failure");
		assert!(foreign.is_empty(), "foreign-label AlreadyPaid/success cannot charge this attempt");
		for (status, active) in [(ListpaysPaysStatus::Pending, true), (ListpaysPaysStatus::Failed, false)] {
			let mut foreign = vec![cln_rpc::ListpaysPays { label: Some("another-attempt".into()),
				payment_hash: hash.to_vec(), status: status as i32, ..Default::default() }];
			assert_eq!(retain_own_pays(&mut foreign, hash, "42").unwrap(), active);
			assert!(foreign.is_empty());
		}
		let mut unknown = vec![cln_rpc::ListpaysPays { payment_hash: hash.to_vec(), status: 99, ..Default::default() }];
		assert!(retain_own_pays(&mut unknown, hash, "42").is_err());
	}

}
