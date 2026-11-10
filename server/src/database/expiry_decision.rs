//! Short durable allocation commits for independently ready, same-expiry outputs.

use std::collections::BTreeMap;

use anyhow::Context;
use bitcoin::{Amount, ScriptBuf, Transaction, TxIn, TxOut, Txid, Sequence, Witness};
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::PublicKey;

use ark::{ServerVtxo, VtxoId};
use chrono::{DateTime, Local};

use bitcoin_ext::{BlockHeight, BlockRef, TxOutExt};
use super::expiry_inventory::BackingSnapshot;
use super::expiry_ownership::{OwnershipBindingSource, OwnershipDisposition, OwnershipEvidence, OwnershipProposal};
use super::expiry_payout::MAX_PAYOUT_VSIZE;
use super::{SpendState, Tx};
use crate::watchman::settlement::{allocate, CustomerLeaf};

pub struct VerifiedPayee {
	pub mailbox: PublicKey,
	pub script: ScriptBuf,
}

pub struct ReadyCoin {
	pub ownership: OwnershipProposal,
	pub payee: Option<VerifiedPayee>,
}

fn group_fits(outputs: &[(ServerVtxo, Vec<ReadyCoin>)]) -> anyhow::Result<bool> {
	// A prior payout can cover the fee for a small later output. Count
	// every destination even when it cannot pay without that history.
	let mut scripts = outputs.iter().flat_map(|o| &o.1)
		.filter_map(|c| c.payee.as_ref().map(|p| p.script.clone())).collect::<Vec<_>>();
	scripts.sort(); scripts.dedup();
	let mut payouts = scripts.into_iter().map(|script_pubkey|
		TxOut { value: Amount::ZERO, script_pubkey }).collect::<Vec<_>>();
	// Every address accepted for the operator drain has a script no
	// larger than native P2WSH/P2TR. Leave room for fee inputs.
	payouts.push(TxOut { value: Amount::ZERO, script_pubkey: ScriptBuf::new_p2wsh(&bitcoin::WScriptHash::all_zeros()) });
	let tx = Transaction {
		version: bitcoin::transaction::Version::TWO, lock_time: bitcoin::absolute::LockTime::ZERO,
		input: outputs.iter().map(|o| &o.0).map(|root| TxIn { previous_output: root.point(),script_sig: ScriptBuf::new(),sequence: Sequence::ZERO,witness: Witness::new() }).collect(),
		output: payouts,
	};
	let mut weight = tx.weight().to_wu().checked_add(4_000)
		.context("expiry group weight overflow")?;
	for root in outputs.iter().map(|o| &o.0) {
		let witness = root.policy().clauses(root.exit_delta(),root.expiry_height(),root.server_pubkey())
			.iter().map(|clause| clause.witness_size(root)).max().unwrap_or(66).max(66);
		weight = weight.checked_add(u64::try_from(witness)?).context("expiry group witness overflow")?;
	}
	Ok(weight.div_ceil(4) <= u64::try_from(MAX_PAYOUT_VSIZE)?)
}

pub struct CommittedExpiryGroup {
	pub id: i64,
	pub inputs: usize,
	pub dropped: usize,
}

fn evidence_json(evidence: &OwnershipEvidence) -> serde_json::Value {
	match evidence {
		OwnershipEvidence::IrreversibleEntitlement => serde_json::json!({"kind":"irreversible_entitlement"}),
		OwnershipEvidence::Pubkey => serde_json::json!({"kind":"pubkey"}),
		OwnershipEvidence::InputOwner => serde_json::json!({"kind":"input_owner"}),
		OwnershipEvidence::LightningReceiveClaim => serde_json::json!({"kind":"lightning_receive_claim"}),
		OwnershipEvidence::Pool => serde_json::json!({"kind":"pool"}),
		OwnershipEvidence::Padding => serde_json::json!({"kind":"padding"}),
		OwnershipEvidence::Forfeit => serde_json::json!({"kind":"forfeit"}),
		OwnershipEvidence::Offboard => serde_json::json!({"kind":"offboard"}),
		OwnershipEvidence::Exchange { .. } => serde_json::json!({"kind":"exchange"}),
		OwnershipEvidence::HtlcStoredPreimage => serde_json::json!({"kind":"htlc_stored_preimage"}),
		OwnershipEvidence::HtlcSourceDischarged => serde_json::json!({"kind":"htlc_source_discharged"}),
		OwnershipEvidence::HtlcExternalFailed => serde_json::json!({"kind":"htlc_external_failed"}),
		OwnershipEvidence::HtlcInternalGrantsReclaimed { block, .. } => serde_json::json!({
			"kind":"htlc_internal_grants_reclaimed","height":block.height.to_u32(),"hash":block.hash.to_string(),
		}),
		OwnershipEvidence::HtlcRecvExpired => serde_json::json!({"kind":"htlc_recv_expired"}),
		OwnershipEvidence::HtlcUnattemptedExpired => serde_json::json!({"kind":"htlc_unattempted_expired"}),
		OwnershipEvidence::HtlcOffchain => serde_json::json!({"kind":"htlc_offchain"}),
	}
}

/// Phase A's analysis of one same-expiry group, written by phase B only if
/// every fact it depends on is unchanged under phase B's locks.
pub struct ExpiryGroupPlan {
	accepted: Vec<(ServerVtxo, Vec<ReadyCoin>)>,
	branch: ExpiryBranchState,
	existing_entitlements: BTreeMap<VtxoId, i64>,
	dropped: usize,
}

/// Every row of the accepted branches, and every participation and Lightning
/// attempt that competes for them. Any change to a vtxo row bumps updated_at.
#[derive(PartialEq)]
struct ExpiryBranchState {
	rows: Vec<(VtxoId, DateTime<Local>, SpendState, Option<BlockHeight>, Option<Txid>)>,
	parts: Vec<i64>,
	attempts: Vec<(i64, Option<i64>)>,
}

impl Tx<'_> {
	async fn expiry_branch_state(&self, roots: &[ServerVtxo]) -> anyhow::Result<(BackingSnapshot, ExpiryBranchState)> {
		let snapshot = self.backing_subtree_snapshot(roots).await?;
		let mut ids = snapshot.regular.iter().map(|fact| fact.state.vtxo_id.to_string()).collect::<Vec<_>>();
		let hashes = snapshot.regular.iter().filter_map(|fact| fact.full_vtxo.as_ref().and_then(|vtxo| vtxo.unlock_hash()))
			.map(|hash| hash.to_string()).collect::<Vec<_>>();
		let parts = self.query("SELECT DISTINCT p.id FROM round_participation p \
			LEFT JOIN round_part_input i ON i.participation_id=p.id \
			WHERE i.vtxo_id=ANY($1) OR p.unlock_hash=ANY($2) ORDER BY p.id", &[&ids, &hashes]).await?
			.into_iter().map(|row| row.get::<_, i64>("id")).collect::<Vec<_>>();
		ids.extend(self.query("SELECT vtxo_id FROM round_part_input WHERE participation_id=ANY($1)", &[&parts]).await?
			.into_iter().map(|row| row.get::<_, String>("vtxo_id")));
		let attempts = self.query("SELECT DISTINCT a.id,a.lightning_htlc_subscription_id FROM lightning_payment_attempt a \
			JOIN lightning_payment_attempt_htlc_vtxo i ON i.lightning_payment_attempt_id=a.id WHERE i.vtxo_id=ANY($1) ORDER BY a.id", &[&ids]).await?
			.into_iter().map(|row| (row.get(0), row.get(1))).collect();
		let rows = snapshot.regular.iter().map(|fact| (fact.state.vtxo_id, fact.state.updated_at, fact.state.spend_state,
			fact.onchain_spent_height, fact.onchain_spent_txid)).collect();
		Ok((snapshot, ExpiryBranchState { rows, parts, attempts }))
	}

	async fn expiry_entitlement(&self, id: VtxoId) -> anyhow::Result<Option<(i64, bool)>> {
		Ok(self.query_opt("SELECT e.id,EXISTS(SELECT 1 FROM expiry_allocation_coin c \
			JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE c.entitlement_id=e.id \
			AND a.status<>'conflicted') AS reserved, \
			EXISTS(SELECT 1 FROM expiry_terminal_claim p WHERE p.entitlement_id=e.id) AS claimed \
			FROM expiry_entitlement e WHERE e.vtxo_id=$1", &[&id.to_string()]).await?
			.map(|row| (row.get("id"), !row.get::<_, bool>("reserved") && !row.get::<_, bool>("claimed"))))
	}

	/// Phase A: resolve the group's current branches in one consistent read,
	/// without row or settlement locks.
	pub async fn plan_expiry_group(
		&self, view: BlockRef, roots: Vec<ServerVtxo>,
	) -> anyhow::Result<ExpiryGroupPlan> {
		self.batch_execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").await?;
		ensure!(roots.iter().all(|root| root.expiry_height() == roots[0].expiry_height() && root.expiry_height() <= view.height),
			"expiry allocation mixes heights or is not mature");
		let mut ready: Vec<(ServerVtxo, Vec<ReadyCoin>)> = Vec::new();
		let mut existing_entitlements = BTreeMap::new();
		let mut dropped = 0;
		for root in roots {
			let current = self.query_opt("SELECT vtxo_id FROM vtxo WHERE vtxo_id=$1 AND frontier_at IS NOT NULL \
				AND confirmed_height<=$2 AND onchain_spent_height IS NULL AND onchain_spent_txid IS NULL",
				&[&root.id().to_string(), &i32::try_from(view.height.to_u32())?]).await?;
			if current.is_none() { dropped += 1; continue; }
			let snapshot = self.backing_subtree_snapshot(std::slice::from_ref(&root)).await?;
			let mut coins = Vec::new();
			let mut valid = true;
			for fact in &snapshot.regular {
				if fact.onchain_spent_height.is_some() || fact.onchain_spent_txid.is_some() { valid = false; break; }
				if let Some(txid) = fact.state.oor_spent_txid.filter(|_| fact.state.offboarded_in.is_none() && fact.state.spent_in_round.is_none()) {
					let mut children = snapshot.regular.iter().filter(|child| child.state.vtxo.point().txid == txid);
					let mut amount = children.try_fold(Amount::ZERO, |sum, child| sum.checked_add(child.state.vtxo.amount())
						.context("expiry branch amount overflow"))?;
					if amount != fact.state.vtxo.amount() {
						// A board fee is a value-bearing P2A output, not a child VTXO.
						// Boards retain the validated Full leaf without making its exit
						// publishable. Use its txid-bound geometry only for this check.
						let spend = self.get_virtual_transaction_by_txid(txid).await?
							.context("expiry branch transaction missing")?.signed_tx
							.map(|tx| tx.into_owned()).or_else(|| snapshot.regular.iter()
								.filter(|child| child.state.vtxo.point().txid == txid)
								.filter_map(|child| child.full_vtxo.as_ref())
								.flat_map(|vtxo| vtxo.transactions())
								.find(|step| step.tx.compute_txid() == txid).map(|step| step.tx))
							.context("expiry branch residual lacks signed transaction")?;
						ensure!(spend.compute_txid() == txid && spend.input.len() == 1
							&& spend.input[0].previous_output == fact.state.vtxo.point(),
							"expiry branch residual is not bound to its input");
						amount = spend.output.iter().filter(|output| output.is_p2a_fee_anchor())
							.try_fold(amount, |sum, output| sum.checked_add(output.value)
								.context("expiry branch fee anchor amount overflow"))?;
					}
					ensure!(amount == fact.state.vtxo.amount(), "expiry branch does not conserve its input");
					continue;
				}
				let ownership = self.propose_expiry_ownership(fact, view.height, Some(view.height)).await?;
				let payee = match &ownership.disposition {
					OwnershipDisposition::Customer { .. } => match self.derived_expiry_owner(&ownership).await? {
						super::expiry_attribution::Attribution::Owned { mailbox_pk, spk } => Some(VerifiedPayee { mailbox: mailbox_pk, script: spk }),
						super::expiry_attribution::Attribution::Unresolved => {
							self.observe_expiry_hold(ownership.id, "owner_binding_unresolved"); valid = false; continue;
						},
					},
					OwnershipDisposition::Operator => None,
					OwnershipDisposition::Unresolved(reason) => {
						if ownership.settlement_integrity_hold { self.warn_settlement_integrity(ownership.id, reason); }
						else { self.observe_expiry_hold(ownership.id, reason); }
						valid = false; continue;
					},
				};
				if let Some((entitlement, free)) = self.expiry_entitlement(ownership.id).await? {
					valid &= free;
					existing_entitlements.insert(ownership.id, entitlement);
				}
				coins.push(ReadyCoin { ownership, payee });
			}
			if coins.is_empty() { self.observe_expiry_hold(root.id(), "current_branch_has_no_owned_terminal"); }
			if valid && !coins.is_empty() {
				let cancellations = coins.iter().flat_map(|coin| coin.ownership.cancellations.iter().copied()).collect::<Vec<_>>();
				valid = self.can_cancel_expiry_participations(&cancellations).await?;
			}
			if valid && !coins.is_empty() {
				ready.push((root, coins));
			} else { dropped += 1; }
		}
		let mut accepted = Vec::new();
		for output in ready {
			accepted.push(output);
			if !group_fits(&accepted)? {
				let output = accepted.pop().expect("just inserted");
				if !group_fits(std::slice::from_ref(&output))? {
					self.observe_expiry_hold(output.0.id(), "output_exceeds_standard_tx");
				}
				dropped += 1;
			}
		}
		let roots = accepted.iter().map(|output| output.0.clone()).collect::<Vec<_>>();
		let (_, branch) = self.expiry_branch_state(&roots).await?;
		Ok(ExpiryGroupPlan { accepted, branch, existing_entitlements, dropped })
	}

	/// Phase B: lock the accepted branches, recheck every fact phase A used
	/// under those locks, then commit the allocation and cancellation.
	pub async fn commit_expiry_group(
		&self, view: BlockRef, group: ExpiryGroupPlan,
	) -> anyhow::Result<Option<CommittedExpiryGroup>> {
		let ExpiryGroupPlan { accepted, branch, existing_entitlements, dropped, .. } = group;
		if accepted.is_empty() { return Ok(None); }
		self.lock_watchman_tip(view).await?;
		let expiry = accepted[0].0.expiry_height();
		self.execute("SELECT pg_advisory_xact_lock($1,$2)", &[&5_i32, &i32::try_from(expiry.to_u32())?]).await?;
		let ids = branch.rows.iter().map(|row| row.0.to_string()).collect::<Vec<_>>();
		self.query("SELECT vtxo_id FROM vtxo WHERE vtxo_id=ANY($1) ORDER BY vtxo_id FOR UPDATE", &[&ids]).await?;
		let roots = accepted.iter().map(|output| output.0.clone()).collect::<Vec<_>>();
		let (snapshot, current) = self.expiry_branch_state(&roots).await?;
		ensure!(current == branch, "expiry branch changed during analysis");
		let lightning = accepted.iter().flat_map(|output| &output.1).filter(|coin| !matches!(coin.ownership.evidence,
			Some(OwnershipEvidence::Pubkey | OwnershipEvidence::IrreversibleEntitlement | OwnershipEvidence::InputOwner
				| OwnershipEvidence::Pool | OwnershipEvidence::Padding | OwnershipEvidence::Forfeit
				| OwnershipEvidence::Offboard | OwnershipEvidence::Exchange { .. }))).collect::<Vec<_>>();
		if !lightning.is_empty() {
			self.lock_htlc_settlements().await?;
			let mut subscriptions = branch.attempts.iter().filter_map(|attempt| attempt.1)
				.chain(snapshot.regular.iter().filter_map(|fact| fact.lightning_htlc_subscription_id)).collect::<Vec<_>>();
			subscriptions.sort_unstable(); subscriptions.dedup();
			self.query("SELECT id FROM lightning_htlc_subscription WHERE id=ANY($1) ORDER BY id FOR SHARE", &[&subscriptions]).await?;
			for coin in lightning {
				let fact = snapshot.regular.iter().find(|fact| fact.state.vtxo_id == coin.ownership.id)
					.context("expiry coin left its branch")?;
				ensure!(self.propose_expiry_ownership(fact, view.height, Some(view.height)).await? == coin.ownership,
					"expiry Lightning facts changed during analysis");
			}
		}
		for coin in accepted.iter().flat_map(|output| &output.1) {
			ensure!(self.expiry_entitlement(coin.ownership.id).await?
				== existing_entitlements.get(&coin.ownership.id).map(|id| (*id, true)), "expiry entitlement changed during analysis");
		}
		let paid = self.query("
			SELECT c.spk,SUM(c.gross_sat)::BIGINT AS gross,SUM(c.fee_sat)::BIGINT AS fee
			FROM expiry_allocation_coin c JOIN expiry_allocation_state a ON a.id=c.allocation_id
			WHERE a.expiry_height=$1 AND a.status<>'conflicted'
				AND c.disposition='customer' AND c.forfeited_sat=0
			GROUP BY c.spk
		", &[&i32::try_from(expiry.to_u32())?]).await?.into_iter().map(|row| {
			Ok((ScriptBuf::from_hex(row.get("spk"))?, (
				Amount::from_sat(u64::try_from(row.get::<_, i64>("gross"))?),
				Amount::from_sat(u64::try_from(row.get::<_, i64>("fee"))?),
			)))
		}).collect::<anyhow::Result<BTreeMap<_, _>>>()?;
		let leaves = accepted.iter().flat_map(|o| &o.1).filter_map(|c| c.payee.as_ref().map(|payee| CustomerLeaf {
			id: c.ownership.id, script: payee.script.clone(), amount: c.ownership.gross,
		})).collect();
		let plan = allocate(leaves, &paid)?;

		// Final selection precedes every cancellation effect. The branches were
		// locked and rechecked without writes above; an unexpected change in
		// the cancellations now rolls back the entire decision transaction.
		for output in &accepted {
			let cancellations = output.1.iter().flat_map(|c| c.ownership.cancellations.iter().copied()).collect::<Vec<_>>();
			ensure!(self.cancel_expiry_participations(&cancellations).await?, "selected expiry cancellation changed under its locks");
		}
		let policy = plan.iter()
			.map(|a| (a.id,a)).collect::<BTreeMap<_, _>>();
		let id: i64 = self.query_one("INSERT INTO expiry_allocation(expiry_height) VALUES($1) RETURNING id",
			&[&i32::try_from(expiry.to_u32())?]).await?.get(0);
		for output in &accepted {
			self.execute("INSERT INTO expiry_allocation_input(allocation_id,vtxo_id) VALUES($1,$2)", &[&id,&output.0.id().to_string()]).await?;
			for coin in &output.1 {
				let proposal = &coin.ownership;
				let evidence = proposal.evidence.as_ref().context("ready ownership lacks evidence")?;
				let mut observed = evidence_json(evidence);
				if matches!(proposal.disposition, OwnershipDisposition::Customer { .. }) {
					observed["binding_source"] = match &proposal.binding_source {
						OwnershipBindingSource::Coin(id) => serde_json::json!({"kind":"coin","id":id.to_string()}),
						OwnershipBindingSource::InputCoins(ids) => serde_json::json!({"kind":"input_coins",
							"ids":ids.iter().map(ToString::to_string).collect::<Vec<_>>() }),
						OwnershipBindingSource::Subscription(id) => serde_json::json!({"kind":"subscription","id":id}),
						OwnershipBindingSource::Allocation(id) => serde_json::json!({"kind":"allocation","id":id}),
					};
				}
				let entitlement = if let Some(existing) = existing_entitlements.get(&proposal.id) {
					*existing
				} else {
					self.query_one("
						INSERT INTO expiry_entitlement(vtxo_id)
						VALUES($1)
						RETURNING id
					",
						&[&proposal.id.to_string()]).await?.get(0)
				};
				let allocation = policy.get(&proposal.id);
				let fee = allocation.map_or(Amount::ZERO, |a| a.fee).to_sat();
				let payout = allocation.map_or(Amount::ZERO, |a| a.payout).to_sat();
				let forfeited = allocation.map_or(Amount::ZERO, |a| a.forfeited).to_sat();
				let subsidy = allocation.map_or(Amount::ZERO, |a| a.subsidy).to_sat();
				let (mailbox, user_key, script) = match (&proposal.disposition, &coin.payee) {
					(OwnershipDisposition::Customer { user_pubkey, .. }, Some(payee)) => (
						Some(payee.mailbox.to_string()),
						Some(user_pubkey.to_string()),
						Some(payee.script.as_bytes().to_lower_hex_string())
					),
					_ => (None, None, None),
				};
				let disposition = match (&proposal.disposition, &coin.payee) {
					(OwnershipDisposition::Customer { .. }, Some(_)) => "customer",
					(OwnershipDisposition::Operator, None) => "operator",
					_ => bail!("allocation ownership lacks its verified payout destination"),
				};
				self.execute("
					INSERT INTO expiry_allocation_coin(allocation_id,entitlement_id,owner_mailbox,user_pubkey,spk,disposition,gross_sat,fee_sat,payout_sat,forfeited_sat,subsidy_sat,evidence)
					VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12::TEXT::JSONB)
				",
					&[
						&id,
						&entitlement,
						&mailbox,
						&user_key,
						&script,
						&disposition,
						&i64::try_from(proposal.gross.to_sat())?,
						&i64::try_from(fee)?,
						&i64::try_from(payout)?,
						&i64::try_from(forfeited)?,
						&i64::try_from(subsidy)?,
						&observed.to_string()
					]).await?;
				self.execute("UPDATE vtxo SET spend_state='spent',updated_at=NOW() WHERE vtxo_id=$1", &[&proposal.id.to_string()]).await?;
			}
		}
		let inputs = accepted.len();
		Ok(Some(CommittedExpiryGroup { id, inputs, dropped }))
	}


}
