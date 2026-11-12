//! Resolve who owns a selected current source.


use bitcoin::Amount;
use bitcoin::secp256k1::PublicKey;
use bitcoin_ext::BlockHeight;

use ark::{ServerVtxoPolicy, VtxoId};
use ark::lightning::PaymentHash;

use super::expiry_inventory::BackingVtxoFact;
use super::expiry_lightning::LightningSourceState;
use super::ln::LightningHtlcSubscriptionStatus;
use super::htlc_vtxo::{self, HtlcDirection, HtlcResolution};
use super::{SpendState, Tx};

/// A customer is paid to `tr(M)` of the mailbox the coin was born with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipDisposition {
	Customer { mailbox: PublicKey },
	Operator,
	Unresolved(&'static str),
}

/// The irreversible layer records these facts; a chain-only conclusion is
/// always tied to the block that supports it and is rebuilt after invalidation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipEvidence {
	Pubkey,
	Board,
	IrreversibleEntitlement,
	InputOwner,
	Pool,
	Padding,
	Forfeit,
	Offboard,
	Exchange { participation_id: i64, unlock_hash: String, forfeited: bool },
	HtlcOffchain,
	HtlcStoredPreimage,
	HtlcSourceDischarged,
	/// No external payment was attempted, and the admitted refund deadline is
	/// behind both completed views. This is an expiry decision, not a forged
	/// upstream off-chain revocation.
	HtlcUnattemptedExpired,
	HtlcExternalFailed,
	/// Chain-derived closure of the receive funded by these own attempts.
	HtlcInternalGrantsReclaimed { payment_hash: PaymentHash },
	/// An unsettled receive grant past its HTLC expiry and unspent on chain.
	/// The operator takes it back with the expiry sweep of its tree output.
	/// Chain-derived: a later preimage spend conflicts the sweep.
	HtlcRecvExpired,
}

impl OwnershipEvidence {
	/// A decision that rests on Lightning facts is planned again from those
	/// facts after a conflict. Every other decision is repeated as made.
	pub fn replannable(&self) -> bool {
		matches!(self, Self::HtlcOffchain | Self::HtlcStoredPreimage
			| Self::HtlcInternalGrantsReclaimed { .. } | Self::HtlcRecvExpired)
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct OwnershipProposal {
	pub id: VtxoId,
	pub gross: Amount,
	pub disposition: OwnershipDisposition,
	pub evidence: Option<OwnershipEvidence>,
	/// These entire participations must be cancelled atomically at commit.
	/// A proposal never cancels anything during background readiness work.
	pub cancellations: Vec<i64>,
}

pub(crate) struct UnregisteredBoardStatus {
	pub root: VtxoId,
	pub user_pubkey: PublicKey,
	pub mailbox: PublicKey,
	pub onchain_spent: bool,
}

impl OwnershipProposal {
	fn pending(fact: &BackingVtxoFact, reason: &'static str) -> Self {
		Self {
			id: fact.state.vtxo_id, gross: fact.state.vtxo.amount(),
			disposition: OwnershipDisposition::Unresolved(reason), evidence: None,
			cancellations: Vec::new(),
		}
	}
}

impl Tx<'_> {
	/// A board cosigned before funding was broadcast remains payable even if
	/// its holder never uploads the completed exit. Registration adds a child;
	/// that child takes over ownership and disables this root-only decision.
	pub(crate) async fn unregistered_board_owner(&self, id: VtxoId)
		-> anyhow::Result<Option<(PublicKey, PublicKey)>>
	{
		let Some(row) = self.query_opt("SELECT b.user_pubkey,b.owner_mailbox,b.funding_spk \
			FROM board_exit b JOIN vtxo v ON v.vtxo_id=b.funding_outpoint \
			WHERE b.funding_outpoint=$1 AND b.exit_txid=v.oor_spent_txid \
			AND v.spend_state='spent' AND b.user_pubkey IS NOT NULL \
			AND b.owner_mailbox IS NOT NULL AND NOT EXISTS (SELECT 1 FROM vtxo child \
			WHERE child.anchor_point=b.funding_outpoint AND child.vtxo_id<>b.funding_outpoint)",
			&[&id.to_string()]).await? else { return Ok(None); };
		let root = self.get_server_vtxo_by_id(id).await?.vtxo;
		let user_pubkey: PublicKey = row.get::<_, &str>("user_pubkey").parse()?;
		let combined = ark::musig::combine_keys([user_pubkey, root.server_pubkey()])
			.x_only_public_key().0;
		if root.point() != root.chain_anchor()
			|| root.policy() != &ServerVtxoPolicy::new_expiry(combined)
			|| root.txout().script_pubkey.as_bytes() != row.get::<_, &[u8]>("funding_spk") {
			return Ok(None);
		}
		Ok(Some((user_pubkey,
			row.get::<_, &str>("owner_mailbox").parse()?)))
	}

	pub(crate) async fn unregistered_board_status(&self, id: VtxoId)
		-> anyhow::Result<Option<UnregisteredBoardStatus>>
	{
		if id.to_point().vout != ark::board::BOARD_FUNDING_TX_VTXO_VOUT {
			return Ok(None);
		}
		let rows = self.query("SELECT b.funding_outpoint, \
			(v.onchain_spent_height IS NOT NULL OR v.onchain_spent_txid IS NOT NULL) AS onchain_spent \
			FROM board_exit b JOIN vtxo v ON v.vtxo_id=b.funding_outpoint \
			WHERE b.exit_txid=$1", &[&id.to_point().txid.to_string()]).await?;
		ensure!(rows.len() <= 1, "board exit txid has conflicting funding outputs");
		let Some(row) = rows.first() else { return Ok(None); };
		let root: VtxoId = row.get::<_, &str>("funding_outpoint").parse()?;
		let Some((user_pubkey, mailbox)) = self.unregistered_board_owner(root).await? else {
			return Ok(None);
		};
		Ok(Some(UnregisteredBoardStatus {
			root, user_pubkey, mailbox, onchain_spent: row.get("onchain_spent"),
		}))
	}

	pub(crate) fn observe_expiry_hold(&self, id: VtxoId, reason: &str) {
		slog!(ExpiryHoldObserved, vtxo_id: id, reason: reason.to_owned());
	}

	/// A customer coin without a recorded owner is held, never an error,
	/// so the other roots are still decided.
	async fn coin_customer(&self, id: VtxoId) -> anyhow::Result<OwnershipDisposition> {
		Ok(match self.coin_owner_mailboxes(&[id]).await?.remove(&id) {
			Some(mailbox) => OwnershipDisposition::Customer { mailbox },
			None => OwnershipDisposition::Unresolved("owner_binding_unresolved"),
		})
	}

	/// Resolve one current economic terminal from the caller's snapshot.
	pub async fn propose_expiry_ownership(
		&self,
		fact: &BackingVtxoFact,
		tip: BlockHeight,
	) -> anyhow::Result<OwnershipProposal> {
		let mut proposal = OwnershipProposal::pending(fact, "unresolved source ownership");
		let full = &fact.state.vtxo;
		let state = &fact.state;
		if let Some((_, mailbox)) = self.unregistered_board_owner(state.vtxo_id).await? {
			proposal.disposition = OwnershipDisposition::Customer { mailbox };
			proposal.evidence = Some(OwnershipEvidence::Board);
			return Ok(proposal);
		}

		if let Some(txid) = state.offboarded_in {
			ensure!(self.query_opt("SELECT txid FROM offboards WHERE txid=$1", &[&txid.to_string()]).await?.is_some(),
				"expiry offboard reference has no durable transaction");
			// A stored offboard is still a customer liability until its payment
			// has six confirmations on the chain seen by both workers.
			let confirmed_before = i32::try_from(tip.to_u32().saturating_sub(5))?;
			let paid = self.query_opt("SELECT n.txid FROM nursery_tx n \
				JOIN captaind_block c ON c.height=n.confirmed_at_height \
				JOIN watchmand_block w ON w.height=c.height AND w.hash=c.hash \
				WHERE n.txid=$1 AND n.kind='offboard' AND n.confirmed_at_height BETWEEN 0 AND $2",
				&[&txid.to_string(), &confirmed_before]).await?.is_some();
			if !paid {
				return Ok(OwnershipProposal::pending(fact, "offboard not confirmed"));
			}
			proposal.disposition = OwnershipDisposition::Operator;
			proposal.evidence = Some(OwnershipEvidence::Offboard);
			return Ok(proposal);
		}

		// A conflicted payout keeps its original spend fence. Replanning
		// reads the already committed irreversible owner rather than treating
		// our own "spent" status as proof that the customer disappeared.
		if state.spend_state == SpendState::Spent {
			if let Some(row) = self.query_opt("
				SELECT c.disposition,c.owner_mailbox,c.replannable
				FROM expiry_allocation_coin c WHERE c.vtxo_id=$1 ORDER BY c.allocation_id LIMIT 1
			", &[&state.vtxo_id.to_string()]).await? {
				if !row.get::<_, bool>("replannable") {
					proposal.disposition = if row.get::<_, &str>("disposition") == "customer" {
						let mailbox: &str = row.get("owner_mailbox");
						OwnershipDisposition::Customer { mailbox: mailbox.parse()? }
					} else if row.get::<_, &str>("disposition") == "operator" {
						OwnershipDisposition::Operator
					} else { return Ok(proposal); };
					proposal.evidence = Some(OwnershipEvidence::IrreversibleEntitlement);
					return Ok(proposal);
				}
			}
		}

		// Bind originals to the particular stored round they fund; a superseded
		// cancelled pending participation is not a permanent coin blocklist.
		let parts = self.query(
			"
				SELECT p.id, p.unlock_hash, p.forfeited_at IS NOT NULL AS forfeited,
				EXISTS(SELECT 1 FROM round_part_input i2 WHERE i2.participation_id=p.id
					AND i2.signed_forfeit_tx IS NOT NULL) AS any_forfeit
			FROM round_part_input i JOIN round_participation p ON p.id=i.participation_id
			LEFT JOIN round r ON r.funding_txid=p.round_id
			WHERE i.vtxo_id=$1 AND (p.round_id IS NULL OR r.id=$2) ORDER BY p.id DESC LIMIT 2
			",
			&[&state.vtxo_id.to_string(), &state.spent_in_round],
		).await?;
		ensure!(parts.len() <= 1, "expiry input has conflicting round participations");
		if let Some(part) = parts.first() {
			let forfeited = part.get::<_, bool>("forfeited");
			if !forfeited && part.get::<_, bool>("any_forfeit") {
				proposal.disposition = OwnershipDisposition::Unresolved("partially stored forfeits");
				return Ok(proposal);
			}
			let id = part.get::<_, i64>("id");
			proposal.evidence = Some(OwnershipEvidence::Exchange {
				participation_id: id, unlock_hash: part.get("unlock_hash"), forfeited,
			});
			if forfeited {
				proposal.disposition = OwnershipDisposition::Operator;
			} else if full.clone().try_into_user_vtxo().is_ok() {
				proposal.disposition = self.coin_customer(state.vtxo_id).await?;
				proposal.cancellations.push(id);
			}
			return Ok(proposal);
		}

		if state.spend_state == SpendState::Pool {
			// Pool is assigned only by the trusted server's own issuance and
			// allocation paths, including dust change removed from the live pool.
			proposal.disposition = OwnershipDisposition::Operator;
			proposal.evidence = Some(OwnershipEvidence::Pool);
			return Ok(proposal);
		}
		if matches!(state.spend_state, SpendState::RoundForfeit | SpendState::OffboardForfeit | SpendState::OffboardConnector) {
			proposal.disposition = OwnershipDisposition::Operator;
			proposal.evidence = Some(OwnershipEvidence::Forfeit);
			return Ok(proposal);
		}
		let Ok(user) = full.clone().try_into_user_vtxo() else { return Ok(proposal); };
		if matches!(user.policy(), ark::VtxoPolicy::Pubkey(policy) if policy.user_pubkey == *crate::round::UNSPENDABLE) {
			proposal.disposition = OwnershipDisposition::Operator;
			proposal.evidence = Some(OwnershipEvidence::Padding);
			return Ok(proposal);
		}
		if state.spend_state == SpendState::Unregistered {
			if fact.full_vtxo.is_some() {
				if let Some(mailbox) = self.expiry_input_owner(state.vtxo_id).await? {
					proposal.disposition = OwnershipDisposition::Customer { mailbox };
					proposal.evidence = Some(OwnershipEvidence::InputOwner);
				}
			}
			return Ok(proposal);
		}
		if state.spend_state == SpendState::Unclaimed {
			let Some(hash) = fact.full_vtxo.as_ref().and_then(|vtxo| vtxo.unlock_hash()) else { return Ok(proposal); };
			let part = self.query_opt("SELECT id, forfeited_at IS NOT NULL AS forfeited FROM round_participation WHERE unlock_hash=$1", &[&hash.to_string()]).await?;
			if let Some(part) = part {
				let id = part.get::<_, i64>("id");
				let forfeited = part.get::<_, bool>("forfeited");
				proposal.disposition = if forfeited { self.coin_customer(state.vtxo_id).await? }
					else { proposal.cancellations.push(id); OwnershipDisposition::Operator };
				proposal.evidence = Some(OwnershipEvidence::Exchange { participation_id: id, unlock_hash: hash.to_string(), forfeited });
			} else if let Some(cancelled) = self.query_opt(
				"SELECT participation_id FROM expiry_cancelled_participation WHERE unlock_hash=$1",
				&[&hash.to_string()],
			).await? {
				// Upstream removes superseded participation rows. Its old
				// unclaimed output remains nonbinding: the durable cancellation
				// also prevents its unlock preimage from ever being released.
				proposal.disposition = OwnershipDisposition::Operator;
				proposal.evidence = Some(OwnershipEvidence::Exchange {
					participation_id: cancelled.get("participation_id"),
					unlock_hash: hash.to_string(), forfeited: false,
				});
			}
			return Ok(proposal);
		}
		if state.oor_spent_txid.is_some() {
			proposal.disposition = OwnershipDisposition::Unresolved("terminal has registered descendants");
			return Ok(proposal);
		}
		let payment = match user.policy() {
			ark::VtxoPolicy::ServerHtlcSend(p) => Some(p.payment_hash),
			ark::VtxoPolicy::ServerHtlcSend_v0(p) => Some(p.payment_hash),
			ark::VtxoPolicy::ServerHtlcRecv(p) => Some(p.payment_hash),
			ark::VtxoPolicy::ServerHtlcRecv_v0(p) => Some(p.payment_hash),
			_ => None,
		};
		if let Some(hash) = payment {
			let htlcs = htlc_vtxo::get_htlc_vtxo_states_by_payment_hash(self, hash).await?;
			let Some(htlc) = htlcs.iter().find(|h| h.vtxo.vtxo_id == proposal.id) else { return Ok(proposal); };
			let has_stored_preimage = self.get_htlc_settlement_by_payment_hash(hash).await?.is_some();
			let source = if htlc.direction() == HtlcDirection::Incoming {
				self.lightning_source_state(state.vtxo_id, hash).await?
			} else { LightningSourceState::Pending };
			let expired_contract = htlc.htlc.htlc_expiry <= tip
				&& fact.onchain_spent_height.is_none() && fact.onchain_spent_txid.is_none();
			let Some(resolution) = htlc.htlc.resolution() else {
				if htlc.direction() == HtlcDirection::Outgoing {
					let Some(subscription_id) = fact.lightning_htlc_subscription_id else { return Ok(proposal); };
					let Some(subscription) = self.get_htlc_subscription_by_id(subscription_id).await? else { return Ok(proposal); };
					if subscription.payment_hash != hash { return Ok(proposal); }
					match subscription.status {
						LightningHtlcSubscriptionStatus::Settled => {
							// Until its claim output is registered, the grant pays the
							// mailbox it was born with: the receiver's.
							proposal.disposition = self.coin_customer(state.vtxo_id).await?;
							proposal.evidence = Some(OwnershipEvidence::HtlcStoredPreimage);
						},
						LightningHtlcSubscriptionStatus::Canceled if expired_contract => {
							proposal.disposition = OwnershipDisposition::Operator;
							proposal.evidence = Some(OwnershipEvidence::HtlcRecvExpired);
						},
						LightningHtlcSubscriptionStatus::HtlcsReady if expired_contract && !has_stored_preimage => {
							proposal.disposition = OwnershipDisposition::Operator;
							proposal.evidence = Some(OwnershipEvidence::HtlcRecvExpired);
						},
						_ => proposal.disposition = OwnershipDisposition::Unresolved("Lightning receive remains open"),
					}
					return Ok(proposal);
				}
				if source == LightningSourceState::Discharged {
					proposal.disposition = OwnershipDisposition::Operator;
					proposal.evidence = Some(OwnershipEvidence::HtlcSourceDischarged);
					return Ok(proposal);
				}
				if source == LightningSourceState::Failed && fact.onchain_spent_height.is_none()
					&& fact.onchain_spent_txid.is_none() && htlc.htlc.chain_resolution.is_none() {
					// An issued internal receive still commits its funding source
					// until every grant is reclaimed, even past the source's CLTV.
					match self.internal_receive_closed(state.vtxo_id, hash, &htlcs, tip).await? {
						Some(true) => {
							proposal.disposition = self.coin_customer(state.vtxo_id).await?;
							proposal.evidence = Some(OwnershipEvidence::HtlcInternalGrantsReclaimed { payment_hash: hash });
							return Ok(proposal);
						},
						Some(false) => {
							proposal.disposition = OwnershipDisposition::Unresolved("internal receive grants not reclaimed");
							return Ok(proposal);
						},
						None => {},
					}
				}
				// A stored preimage next to a failed payment through the
				// Lightning node may be that payment having gone through after
				// all. Hold the source unless another payment or a settled
				// receive explains the preimage. A source never sent to the
				// node, or only to an intra-Ark receive, cannot have paid.
				if source == LightningSourceState::Failed && has_stored_preimage
					&& self.query_one("SELECT EXISTS(SELECT 1 FROM lightning_payment_attempt a \
						JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id \
						WHERE l.vtxo_id=$1 AND a.payment_hash=$2 AND a.status='failed' \
						AND a.lightning_htlc_subscription_id IS NULL) \
						AND NOT EXISTS(SELECT 1 FROM lightning_payment_attempt a \
							WHERE a.payment_hash=$2 AND a.status='succeeded') \
						AND NOT EXISTS(SELECT 1 FROM lightning_htlc_subscription s \
							WHERE s.payment_hash=$2 AND s.status='settled')",
						&[&state.vtxo_id.to_string(), &hash.to_string()]).await?.get::<_, bool>(0) {
					proposal.disposition = OwnershipDisposition::Unresolved("failed Lightning payment has a stored preimage");
					return Ok(proposal);
				}
				// A same-hash receive funded by another source cannot retain
				// this sender's money. Open attempts still wait.
				if matches!(source, LightningSourceState::Unattempted | LightningSourceState::Failed)
					&& fact.onchain_spent_height.is_none() && htlc.htlc.chain_resolution.is_none()
					&& htlc.htlc.htlc_expiry <= tip {
					proposal.disposition = self.coin_customer(state.vtxo_id).await?;
					proposal.evidence = Some(match source {
						LightningSourceState::Failed => OwnershipEvidence::HtlcExternalFailed,
						_ => OwnershipEvidence::HtlcUnattemptedExpired,
					});
				} else {
					proposal.disposition = OwnershipDisposition::Unresolved("Lightning settlement pending");
				}
				return Ok(proposal);
			};
			if htlc.direction() == HtlcDirection::Outgoing && resolution == HtlcResolution::Revoked
				&& !htlc_vtxo::payout_htlc_recvs_expired(&htlcs, tip, Some(tip)) {
				proposal.disposition = OwnershipDisposition::Unresolved("Lightning receive not irrevocably expired");
				return Ok(proposal);
			}
			if htlc.htlc.chain_resolution.is_some() { return Ok(proposal); }
			proposal.evidence = Some(OwnershipEvidence::HtlcOffchain);
			proposal.disposition = match (htlc.direction(), resolution) {
				(HtlcDirection::Incoming, HtlcResolution::Fulfilled) | (HtlcDirection::Outgoing, HtlcResolution::Revoked) => OwnershipDisposition::Operator,
				_ => self.coin_customer(state.vtxo_id).await?,
			};
			return Ok(proposal);
		}
		if state.spend_state != SpendState::Spendable {
			proposal.disposition = OwnershipDisposition::Unresolved("nonterminal spend state without settlement evidence");
			return Ok(proposal);
		}
		proposal.disposition = self.coin_customer(state.vtxo_id).await?;
		proposal.evidence = Some(OwnershipEvidence::Pubkey);
		Ok(proposal)
	}

}
