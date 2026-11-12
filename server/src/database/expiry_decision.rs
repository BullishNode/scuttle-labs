//! Short durable allocation commits, one per independently ready expired root.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use bitcoin::{Amount, Txid};

use ark::{ServerVtxo, VtxoId};
use chrono::{DateTime, Local};

use bitcoin_ext::{BlockHeight, BlockRef};
use super::expiry_inventory::BackingSnapshot;
use super::expiry_ownership::{OwnershipDisposition, OwnershipEvidence, OwnershipProposal};
use super::{SpendState, Tx};
use crate::watchman::settlement::{allocate, CustomerLeaf};

/// Phase A's analysis of one expired root, written by phase B only if
/// every fact it depends on is unchanged under phase B's locks.
pub struct ExpiryRootPlan {
	root: ServerVtxo,
	coins: Vec<OwnershipProposal>,
	branch: ExpiryBranchState,
	decided: BTreeSet<VtxoId>,
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

	/// `None` if the coin was never decided, else whether it is free for a
	/// new decision: every allocation that holds it has conflicted.
	async fn earlier_decision_free(&self, id: VtxoId) -> anyhow::Result<Option<bool>> {
		Ok(self.query_opt("SELECT BOOL_AND(a.status='conflicted') AS free FROM expiry_allocation_coin c \
			JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE c.vtxo_id=$1 HAVING COUNT(*)>0",
			&[&id.to_string()]).await?.map(|row| row.get("free")))
	}

	/// Phase A: resolve the root's current branch in one consistent read,
	/// without row or settlement locks. None if the root is not ready.
	pub async fn plan_expiry_root(
		&self, view: BlockRef, root: ServerVtxo,
	) -> anyhow::Result<Option<ExpiryRootPlan>> {
		self.batch_execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ").await?;
		ensure!(root.expiry_height() <= view.height, "expiry root is not mature");
		let mut decided = BTreeSet::new();
		let current = self.query_opt("SELECT vtxo_id FROM vtxo WHERE vtxo_id=$1 AND frontier_at IS NOT NULL \
			AND confirmed_height<=$2 AND onchain_spent_height IS NULL AND onchain_spent_txid IS NULL",
			&[&root.id().to_string(), &i32::try_from(view.height.to_u32())?]).await?;
		if current.is_none() { return Ok(None); }
		let snapshot = self.backing_subtree_snapshot(std::slice::from_ref(&root)).await?;
		let mut coins = Vec::new();
		let mut valid = true;
		for fact in &snapshot.regular {
			if fact.onchain_spent_height.is_some() || fact.onchain_spent_txid.is_some() { valid = false; break; }
			// A coin spent off-chain is decided through its children. A cosigned
			// board without registration has none: its funding root is the
			// payable coin. Any other childless spend is held below.
			let spent_offchain = fact.state.oor_spent_txid
				.filter(|_| fact.state.offboarded_in.is_none() && fact.state.spent_in_round.is_none());
			if spent_offchain.is_some_and(|txid| snapshot.regular.iter().any(|child| child.state.vtxo.point().txid == txid)) {
				continue;
			}
			let ownership = self.propose_expiry_ownership(fact, view.height).await?;
			if let OwnershipDisposition::Unresolved(reason) = ownership.disposition {
				self.observe_expiry_hold(ownership.id, reason);
				valid = false; continue;
			}
			if let Some(free) = self.earlier_decision_free(ownership.id).await? {
				valid &= free;
				decided.insert(ownership.id);
			}
			coins.push(ownership);
		}
		// Owners are paid from the root's own value only.
		let owed = coins.iter().filter(|coin| matches!(coin.disposition, OwnershipDisposition::Customer { .. }))
			.try_fold(Amount::ZERO, |sum, coin| sum.checked_add(coin.gross)).context("expiry branch amount overflow")?;
		ensure!(owed <= root.amount(), "expiry branch owes its owners more than its root holds");
		if coins.is_empty() { self.observe_expiry_hold(root.id(), "current_branch_has_no_owned_terminal"); }
		if valid && !coins.is_empty() {
			let cancellations = coins.iter().flat_map(|coin| coin.cancellations.iter().copied()).collect::<Vec<_>>();
			valid = self.can_cancel_expiry_participations(&cancellations).await?;
		}
		if !valid || coins.is_empty() { return Ok(None); }
		let (_, branch) = self.expiry_branch_state(std::slice::from_ref(&root)).await?;
		Ok(Some(ExpiryRootPlan { root, coins, branch, decided }))
	}

	/// Phase B: lock the root's branch, recheck every fact phase A used
	/// under those locks, then commit the allocation and cancellation.
	/// Returns the allocation id.
	pub async fn commit_expiry_root(
		&self, view: BlockRef, plan: ExpiryRootPlan,
	) -> anyhow::Result<i64> {
		let ExpiryRootPlan { root, coins, branch, decided } = plan;
		self.lock_watchman_tip(view).await?;
		let ids = branch.rows.iter().map(|row| row.0.to_string()).collect::<Vec<_>>();
		self.query("SELECT vtxo_id FROM vtxo WHERE vtxo_id=ANY($1) ORDER BY vtxo_id FOR UPDATE", &[&ids]).await?;
		let (snapshot, current) = self.expiry_branch_state(std::slice::from_ref(&root)).await?;
		ensure!(current == branch, "expiry branch changed during analysis");
		let lightning = coins.iter().filter(|coin| !matches!(coin.evidence,
			Some(OwnershipEvidence::Pubkey | OwnershipEvidence::Board | OwnershipEvidence::IrreversibleEntitlement | OwnershipEvidence::InputOwner
				| OwnershipEvidence::Pool | OwnershipEvidence::Padding | OwnershipEvidence::Forfeit
				| OwnershipEvidence::Offboard | OwnershipEvidence::Exchange { .. }))).collect::<Vec<_>>();
		if !lightning.is_empty() {
			self.lock_htlc_settlements().await?;
			let mut subscriptions = branch.attempts.iter().filter_map(|attempt| attempt.1)
				.chain(snapshot.regular.iter().filter_map(|fact| fact.lightning_htlc_subscription_id)).collect::<Vec<_>>();
			subscriptions.sort_unstable(); subscriptions.dedup();
			self.query("SELECT id FROM lightning_htlc_subscription WHERE id=ANY($1) ORDER BY id FOR SHARE", &[&subscriptions]).await?;
			for coin in lightning {
				let fact = snapshot.regular.iter().find(|fact| fact.state.vtxo_id == coin.id)
					.context("expiry coin left its branch")?;
				ensure!(self.propose_expiry_ownership(fact, view.height).await? == *coin,
					"expiry Lightning facts changed during analysis");
			}
		}
		for coin in &coins {
			ensure!(self.earlier_decision_free(coin.id).await? == decided.contains(&coin.id).then_some(true),
				"earlier expiry decision changed during analysis");
		}
		// Each owner pays one fee for this root, whatever else expires at
		// the same height.
		let leaves = coins.iter().filter_map(|c| match c.disposition {
			OwnershipDisposition::Customer { mailbox } => Some(CustomerLeaf {
				id: c.id, script: ark::fallback::payout_script(mailbox), amount: c.gross,
			}),
			_ => None,
		}).collect();
		let amounts = allocate(leaves)?;

		// Final selection precedes every cancellation effect. The branch was
		// locked and rechecked without writes above; an unexpected change in
		// the cancellations now rolls back the entire decision transaction.
		let cancellations = coins.iter().flat_map(|c| c.cancellations.iter().copied()).collect::<Vec<_>>();
		ensure!(self.cancel_expiry_participations(&cancellations).await?, "selected expiry cancellation changed under its locks");
		let policy = amounts.iter()
			.map(|a| (a.id,a)).collect::<BTreeMap<_, _>>();
		let id: i64 = self.query_one("INSERT INTO expiry_allocation(expiry_height) VALUES($1) RETURNING id",
			&[&i32::try_from(root.expiry_height().to_u32())?]).await?.get(0);
		self.execute("INSERT INTO expiry_allocation_input(allocation_id,vtxo_id) VALUES($1,$2)", &[&id,&root.id().to_string()]).await?;
		for proposal in &coins {
			let evidence = proposal.evidence.as_ref().context("ready ownership lacks evidence")?;
			let allocation = policy.get(&proposal.id);
			let fee = allocation.map_or(Amount::ZERO, |a| a.fee).to_sat();
			let payout = allocation.map_or(Amount::ZERO, |a| a.payout).to_sat();
			let subsidy = allocation.map_or(Amount::ZERO, |a| a.subsidy).to_sat();
			let (disposition, mailbox) = match proposal.disposition {
				OwnershipDisposition::Customer { mailbox } => ("customer", Some(mailbox.to_string())),
				OwnershipDisposition::Operator => ("operator", None),
				OwnershipDisposition::Unresolved(_) => bail!("allocation ownership is unresolved"),
			};
			self.execute("
				INSERT INTO expiry_allocation_coin(allocation_id,vtxo_id,owner_mailbox,disposition,gross_sat,fee_sat,payout_sat,subsidy_sat,replannable)
				VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)
			",
				&[
					&id,
					&proposal.id.to_string(),
					&mailbox,
					&disposition,
					&i64::try_from(proposal.gross.to_sat())?,
					&i64::try_from(fee)?,
					&i64::try_from(payout)?,
					&i64::try_from(subsidy)?,
					&evidence.replannable(),
				]).await?;
			self.execute("UPDATE vtxo SET spend_state='spent',updated_at=NOW() WHERE vtxo_id=$1", &[&proposal.id.to_string()]).await?;
		}
		Ok(id)
	}
}
