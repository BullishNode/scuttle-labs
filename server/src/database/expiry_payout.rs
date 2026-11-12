//! Exact signed payout parents and their allocated backing.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use bitcoin::{Amount, ScriptBuf, Transaction, TxOut};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::secp256k1::PublicKey;
use bitcoin_ext::{BlockRef, TxOutExt};
use ark::{ProtocolEncoding, ServerVtxo, ServerVtxoPolicy, VtxoId, VtxoPolicy};

use super::expiry_lightning::LightningSourceState;
use super::htlc_vtxo::{self, HtlcDirection, HtlcResolution};
use super::ln::LightningHtlcSubscriptionStatus;
use super::Tx;

pub(crate) const MAX_PAYOUT_VSIZE: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackState {
	Pending,
	Paid,
	Forfeited,
}

/// The expiry payout decided for a coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FallbackReceipt {
	pub state: FallbackState,
	pub gross: Amount,
	pub fee: Amount,
	pub payout: Amount,
}

fn receive_timeout_allowed(
	status: LightningHtlcSubscriptionStatus,
	offchain: Option<HtlcResolution>,
	onchain: Option<HtlcResolution>,
	preimage_known: bool,
) -> bool {
	if offchain == Some(HtlcResolution::Fulfilled) || onchain == Some(HtlcResolution::Fulfilled) {
		return false;
	}
	match status {
		LightningHtlcSubscriptionStatus::Canceled => true,
		LightningHtlcSubscriptionStatus::HtlcsReady => !preimage_known,
		_ => false,
	}
}

pub struct PendingExpiryPayout {
	pub id: i64,
	pub inputs: Vec<ServerVtxo>,
	pub payouts: Vec<TxOut>,
	pub parent: Option<Transaction>,
	pub parent_fee: Option<Amount>,
}

impl Tx<'_> {
	/// The payout decided for each coin, keyed by the coin and the mailbox
	/// it was born with. A payout to another mailbox, such as an undelivered
	/// arkoor output refunded to its sender, is not reported to the coin.
	pub(crate) async fn fallback_receipts(&self, owners: &BTreeMap<VtxoId, PublicKey>)
		-> anyhow::Result<BTreeMap<VtxoId, FallbackReceipt>>
	{
		if !self.expiry_payouts() || owners.is_empty() { return Ok(BTreeMap::new()); }
		let ids = owners.keys().map(ToString::to_string).collect::<Vec<_>>();
		// The winning decision of each coin: a confirmed one, else the latest.
		let rows = self.query("SELECT DISTINCT ON (c.vtxo_id) c.vtxo_id,c.disposition,c.owner_mailbox, \
			c.gross_sat,c.fee_sat,c.payout_sat,a.status,v.confirmed_height IS NOT NULL AS exposed, \
			a.confirmed_height+5 <= (SELECT MAX(height) FROM watchmand_block) AS deep \
			FROM expiry_allocation_coin c JOIN expiry_allocation_state a ON a.id=c.allocation_id \
			JOIN vtxo v ON v.vtxo_id=c.vtxo_id \
			WHERE c.vtxo_id=ANY($1) ORDER BY c.vtxo_id,(a.status='confirmed') DESC,c.allocation_id DESC",
			&[&ids]).await?;
		let mut receipts = BTreeMap::new();
		for row in rows {
			let id: VtxoId = row.get::<_, &str>("vtxo_id").parse()?;
			let mailbox = row.get::<_, Option<&str>>("owner_mailbox").map(str::parse::<PublicKey>).transpose()?;
			if row.get::<_, &str>("disposition") != "customer" || mailbox.as_ref() != owners.get(&id) {
				continue;
			}
			// The holder put the coin itself on chain, so no payout follows.
			let status: &str = row.get("status");
			if status == "conflicted" && row.get::<_, bool>("exposed") { continue; }
			let amount = |column| -> anyhow::Result<Amount> {
				Ok(Amount::from_sat(u64::try_from(row.get::<_, i64>(column))?))
			};
			let payout = amount("payout_sat")?;
			let state = if status != "confirmed" || row.get::<_, Option<bool>>("deep") != Some(true) {
				FallbackState::Pending
			} else if payout > Amount::ZERO { FallbackState::Paid } else { FallbackState::Forfeited };
			receipts.insert(id, FallbackReceipt { state, gross: amount("gross_sat")?, fee: amount("fee_sat")?, payout });
		}
		Ok(receipts)
	}

	/// The owner outputs of an allocation: each mailbox's total, to `tr(M)`.
	async fn expiry_owner_outputs(&self, id: i64) -> anyhow::Result<BTreeMap<ScriptBuf, Amount>> {
		self.query("SELECT owner_mailbox,SUM(payout_sat)::BIGINT AS amount_sat FROM expiry_allocation_coin \
			WHERE allocation_id=$1 AND disposition='customer' GROUP BY owner_mailbox HAVING SUM(payout_sat)>0",
			&[&id]).await?.into_iter().map(|row| Ok((
				ark::fallback::payout_script(row.get::<_, &str>("owner_mailbox").parse()?),
				Amount::from_sat(u64::try_from(row.get::<_, i64>("amount_sat"))?),
			))).collect()
	}

	/// Each allocation is one root and is paid by one transaction.
	pub async fn pending_expiry_payouts(&self) -> anyhow::Result<Vec<PendingExpiryPayout>> {
		let rows = self.query("SELECT a.id,a.sweep_tx,p.fee_sat FROM expiry_allocation_state a \
			LEFT JOIN expiry_payout_attempt p ON p.allocation_id=a.id AND p.txid=a.sweep_txid \
			WHERE a.status='pending' ORDER BY a.expiry_height,a.id", &[]).await?;
		let mut ret = Vec::with_capacity(rows.len());
		for row in rows {
			let id: i64 = row.get("id");
			let inputs = self.query("
				SELECT v.vtxo
				FROM expiry_allocation_input i JOIN vtxo v ON v.vtxo_id=i.vtxo_id
				WHERE i.allocation_id=$1
				ORDER BY v.vtxo_id
			", &[&id]).await?
				.into_iter().map(|r| ServerVtxo::deserialize(r.get("vtxo"))).collect::<Result<Vec<_>, _>>()?;
			let payouts = self.expiry_owner_outputs(id).await?.into_iter()
				.map(|(script_pubkey, value)| TxOut { script_pubkey, value }).collect();
			let parent = row.get::<_, Option<&[u8]>>("sweep_tx").map(deserialize).transpose()?;
			let parent_fee = row.get::<_, Option<i64>>("fee_sat")
				.map(|fee| u64::try_from(fee).map(Amount::from_sat)).transpose()?;
			ensure!(parent.is_some() == parent_fee.is_some(),
				"pending payout attempt lacks a recorded fee or transaction");
			ret.push(PendingExpiryPayout { id, inputs, payouts, parent, parent_fee });
		}
		Ok(ret)
	}

	/// Keep every signed replacement and the exact owner outputs before any
	/// network publication. An older attempt can still win on chain.
	pub async fn store_expiry_attempt(
		&self, view: BlockRef, id: i64, payout: &Transaction,
		fee: Amount, operator_spk: &ScriptBuf,
	) -> anyhow::Result<()> {
		self.lock_watchman_tip(view).await?;
		self.query_one("SELECT id FROM expiry_allocation WHERE id=$1 FOR UPDATE", &[&id]).await?;
		ensure!(self.query_one("SELECT status FROM expiry_allocation_state WHERE id=$1", &[&id]).await?.get::<_, &str>(0) == "pending",
			"expiry allocation is no longer pending");
		ensure!(payout.version == bitcoin::transaction::Version::TWO
			&& payout.vsize() <= MAX_PAYOUT_VSIZE
			&& payout.input.iter().any(|input| input.sequence.to_consensus_u32() <= 0xFFFFFFFD)
			&& payout.output.iter().all(TxOutExt::is_standard),
			"direct expiry attempt is nonstandard or lacks RBF");
		let roots = self.query("SELECT v.vtxo FROM expiry_allocation_input i \
			JOIN vtxo v ON v.vtxo_id=i.vtxo_id WHERE i.allocation_id=$1", &[&id]).await?
			.into_iter().map(|row| ServerVtxo::deserialize(row.get("vtxo")))
			.collect::<Result<Vec<ServerVtxo>, _>>()?;
		let backing = roots.iter().map(|root| root.point()).collect::<BTreeSet<_>>();
		let spent = payout.input.iter().map(|input| input.previous_output)
			.collect::<BTreeSet<_>>();
		ensure!(!backing.is_empty() && backing.is_subset(&spent)
			&& spent.len() == payout.input.len(),
			"direct expiry attempt omits or duplicates allocated backing");
		let expected = self.expiry_owner_outputs(id).await?;
		ensure!(!expected.contains_key(operator_spk),
			"operator drain aliases an owner script");
		let owner_outputs = payout.output.iter().filter(|out| expected.contains_key(&out.script_pubkey))
			.collect::<Vec<_>>();
		ensure!(owner_outputs.len() == expected.len() && owner_outputs.iter().all(|out|
			expected.get(&out.script_pubkey) == Some(&out.value)),
			"direct expiry attempt changes owner outputs");
		// Owners are paid from the swept roots only. Watchman inputs may pay
		// the mining fee, never an owner.
		let swept = roots.iter().try_fold(Amount::ZERO, |sum, root| sum.checked_add(root.amount()));
		let owed = expected.values().try_fold(Amount::ZERO, |sum, value| sum.checked_add(*value));
		ensure!(owed.zip(swept).is_some_and(|(owed, swept)| owed <= swept),
			"direct expiry attempt pays owners more than its roots hold");
		let other = payout.output.iter().filter(|out| !expected.contains_key(&out.script_pubkey))
			.collect::<Vec<_>>();
		ensure!(other.len() <= 1 && other.iter().all(|out| &out.script_pubkey == operator_spk),
			"direct expiry attempt pays an unknown script");
		let txid = payout.compute_txid().to_string();
		let raw = serialize(payout);
		let fee_sat = i64::try_from(fee.to_sat())?;
		if let Some(same) = self.query_opt("SELECT signed_tx,fee_sat FROM expiry_payout_attempt \
			WHERE allocation_id=$1 AND txid=$2", &[&id,&txid]).await? {
			ensure!(same.get::<_, &[u8]>("signed_tx") == raw.as_slice() && same.get::<_, i64>("fee_sat") == fee_sat,
				"stored direct expiry attempt changed bytes or fee");
			return Ok(());
		}
		let prior_fee = self.query_one("SELECT MAX(fee_sat) FROM expiry_payout_attempt WHERE allocation_id=$1",
			&[&id]).await?.get::<_, Option<i64>>(0);
		ensure!(prior_fee.is_none_or(|prior| fee_sat > prior),
			"direct expiry replacement does not raise the absolute fee");
		self.execute("INSERT INTO expiry_payout_attempt(allocation_id,txid,signed_tx,fee_sat) \
			VALUES($1,$2,$3,$4)", &[&id,&txid,&raw,&fee_sat]).await?;
		Ok(())
	}

}

impl Tx<'_> {
	/// Shared preimage knowledge cannot authorize an unpaid send's drain claim.
	/// Keep the exact source and its outcome stable through native publication.
	pub async fn lock_watchman_claim_inputs(&self, vtxos: &[ServerVtxo]) -> anyhow::Result<()> {
		if !self.expiry_payouts() { return Ok(()); }
		let ids = vtxos.iter().map(|vtxo| vtxo.id()).collect::<Vec<_>>();
		self.lock_unallocated_watchman_inputs(&ids).await?;
		for candidate in vtxos {
			let (hash, expiry, sending) = match candidate.policy() {
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcSend(policy)) => (policy.payment_hash, policy.htlc_expiry, true),
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcSend_v0(policy)) => (policy.payment_hash, policy.htlc_expiry, true),
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcRecv(policy)) => (policy.payment_hash, policy.htlc_expiry, false),
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcRecv_v0(policy)) => (policy.payment_hash, policy.htlc_expiry, false),
				_ => continue,
			};
			let state = self.get_server_vtxo_by_id(candidate.id()).await?;
			ensure!(state.vtxo_id == candidate.id() && state.vtxo.id() == candidate.id()
				&& state.vtxo.to_bare().serialize() == candidate.to_bare().serialize(),
				"Lightning claim source differs from its retained contract");
			ensure!(state.confirmed_height.is_some() && state.oor_spent_txid.is_none()
				&& state.spent_in_round.is_none() && state.offboarded_in.is_none(),
				"Lightning claim source has a successor or lacks confirmation");
			let unspent = self.query_one("SELECT onchain_spent_height IS NULL AND onchain_spent_txid IS NULL AS unspent \
				FROM vtxo WHERE id=$1", &[&state.id]).await?.get::<_, bool>("unspent");
			ensure!(unspent, "Lightning claim source is already spent on chain");
			let htlc = htlc_vtxo::get_htlc_vtxo(self, candidate.id()).await?
				.context("Lightning claim source has no registered HTLC")?;
			let direction = if sending { HtlcDirection::Incoming } else { HtlcDirection::Outgoing };
			ensure!(htlc.vtxo.vtxo_id == candidate.id() && htlc.htlc.direction == direction
				&& htlc.htlc.payment_hash == hash && htlc.htlc.htlc_expiry == expiry,
				"Lightning claim source has a different HTLC");
			if sending {
				ensure!(htlc.htlc.offchain_resolution != Some(HtlcResolution::Revoked)
					&& htlc.htlc.chain_resolution != Some(HtlcResolution::Revoked),
					"Lightning claim source was revoked");
				ensure!(self.lightning_source_state(candidate.id(), hash).await? == LightningSourceState::Discharged,
					"Lightning source is not discharged");
			} else {
				// A preimage can be durable before the native invoice's Settled update.
				self.lock_htlc_settlements().await?;
				let subscription = self.query_opt("SELECT s.status,s.payment_hash FROM lightning_htlc_subscription s \
					JOIN vtxo v ON v.lightning_htlc_subscription_id=s.id WHERE v.id=$1 FOR UPDATE OF s",
					&[&state.id]).await?.context("receive claim source has no subscription")?;
				ensure!(subscription.get::<_, &str>("payment_hash") == hash.to_string(),
					"receive claim source has a different subscription");
				let preimage_known = self.get_htlc_settlement_by_payment_hash(hash).await?.is_some();
				ensure!(receive_timeout_allowed(subscription.get("status"), htlc.htlc.offchain_resolution,
					htlc.htlc.chain_resolution, preimage_known), "receive grant may have been collected");
			}
		}
		Ok(())
	}

	/// A reaction claim must not replace a financial payout already committed
	/// for its physical backing. Use the allocator's sorted coin lock order and
	/// keep these locks through broadcast, including when the root is not itself
	/// one of the economic customer terminals.
	pub async fn lock_unallocated_watchman_inputs(&self, ids: &[ark::VtxoId]) -> anyhow::Result<()> {
		if !self.expiry_payouts() { return Ok(()); }
		let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
		self.query("SELECT vtxo_id FROM vtxo WHERE vtxo_id=ANY($1) ORDER BY vtxo_id FOR UPDATE", &[&ids]).await?;
		// Physical reservations survive while pending/confirmed. An operator
		// decision stays reserved until all its allocations have conflicted.
		// Customer decisions remain irreversible even across a conflict;
		// the off-chain obligation guard retains its separate stronger fence.
		if let Some(row) = self.query_opt("SELECT vtxo_id FROM ( \
			SELECT i.vtxo_id FROM expiry_allocation_input i JOIN expiry_allocation_state a ON a.id=i.allocation_id \
				WHERE i.vtxo_id=ANY($1) AND a.status<>'conflicted' \
			UNION SELECT c.vtxo_id FROM expiry_allocation_coin c JOIN expiry_allocation_state a ON a.id=c.allocation_id \
				WHERE c.vtxo_id=ANY($1) AND (a.status<>'conflicted' OR c.disposition='customer')) fenced \
			ORDER BY vtxo_id LIMIT 1", &[&ids]).await? {
			return Err(crate::error::FallbackRefusal::DecidedForFallback(row.get::<_, &str>("vtxo_id").parse()?).into());
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use LightningHtlcSubscriptionStatus::{Canceled, Created, Accepted, HtlcsReady, Settled};
	use HtlcResolution::{Fulfilled, Revoked};

	#[test]
	fn receive_timeout_keeps_collected_and_inflight_grants() {
		assert!(receive_timeout_allowed(HtlcsReady, None, None, false));
		assert!(!receive_timeout_allowed(HtlcsReady, None, None, true));
		for known in [false, true] {
			assert!(receive_timeout_allowed(Canceled, None, None, known));
			assert!(receive_timeout_allowed(Canceled, Some(Revoked), None, known));
			for status in [Created, Accepted, HtlcsReady, Settled, Canceled] {
				assert!(!receive_timeout_allowed(status, Some(Fulfilled), None, known));
				assert!(!receive_timeout_allowed(status, None, Some(Fulfilled), known));
			}
			for status in [Created, Accepted, Settled] {
				assert!(!receive_timeout_allowed(status, None, None, known));
			}
		}
	}

}
