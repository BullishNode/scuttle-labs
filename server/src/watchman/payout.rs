//! Immutable zero-fee sweep parents. Fee replacements affect only their child.

use anyhow::Context;
use bitcoin::{Amount, Transaction, TxOut, Txid, Weight};
use bitcoin_ext::TxStatus;
use bitcoin_ext::{fee, TransactionExt, TxOutExt};
use bitcoind_async_client::traits::Reader;
use bitcoin_ext::bdk::WalletExt;
use bitcoin_ext::cpfp::MakeCpfpFees;
use tracing::warn;

use crate::bitcoind as bcd;
use crate::bitcoind::BitcoindErrorExt;
use bitcoin_ext::BlockRef;
use crate::database::expiry_payout::{MAX_PAYOUT_VSIZE, PendingExpiryPayout};

use ark::ServerVtxo;

use super::Watchman;

/// The current anchor spender supplies only Core's replacement fee and inputs.
/// Customer settlement depends on the immutable parent, not its fee sponsor.
pub(super) async fn mempool_child_fee(
	client: &bitcoind_async_client::Client, txid: Txid,
) -> anyhow::Result<Option<(Amount, Weight)>> {
	#[derive(Debug, serde::Deserialize)]
	struct Entry { fees: Fees, weight: u64 }
	#[derive(Debug, serde::Deserialize)]
	struct Fees {
		#[serde(with = "bitcoin::amount::serde::as_btc")]
		base: Amount,
	}
	let entry: Entry = match client.call_raw("getmempoolentry", &[bcd::json_arg(txid)?]).await {
		Ok(entry) => entry,
		Err(error) if error.is_not_found() => return Ok(None),
		Err(error) => return Err(error.into()),
	};
	Ok(Some((entry.fees.base, Weight::from_wu(entry.weight))))
}

impl Watchman {
	/// Build the complete group as one transaction; callers must never feed a
	/// committed group through process_claims, which retries individual inputs.
	pub(crate) async fn build_expiry_payout(
		&self,
		vtxos: &[ServerVtxo],
		payouts: &[TxOut],
	) -> anyhow::Result<Transaction> {
		ensure!(!vtxos.is_empty(), "expiry payout has no backing inputs");
		ensure!(payouts.iter().all(|out| out.is_standard()), "expiry payout contains nonstandard or dust payee output");
		let input_amount = vtxos.iter().try_fold(Amount::ZERO, |sum, vtxo|
			sum.checked_add(vtxo.amount()).context("claim input amount overflow"),
		)?;
		let paid = payouts.iter().try_fold(Amount::ZERO, |sum, out|
			sum.checked_add(out.value).context("expiry payout amount overflow"),
		)?;
		let remainder = input_amount.checked_sub(paid).context("expiry payout exceeds backing")?;
		let mut outputs = payouts.to_vec();
		if remainder >= self.drain_spk.minimal_non_dust() {
			outputs.push(TxOut { value: remainder, script_pubkey: self.drain_spk.clone() });
			outputs.push(fee::fee_anchor());
		} else {
			// One ephemeral dust output, consumed by the fee child, returns a
			// sub-dust operator remainder without a nonstandard drain output.
			outputs.push(fee::fee_anchor_with_amount(remainder));
		}
		let tx = self.build_claim_tx(vtxos, bitcoin::FeeRate::ZERO, Some(outputs)).await?;
		ensure!(tx.vsize() <= MAX_PAYOUT_VSIZE, "expiry payout requires splitting: {} vbytes", tx.vsize());
		let outputs = tx.output.iter().try_fold(Amount::ZERO, |sum, out|
			sum.checked_add(out.value).context("expiry payout output overflow"),
		)?;
		ensure!(outputs == input_amount, "expiry sweep parent must have exactly zero fee");
		Ok(tx)
	}

	/// Publish only already committed groups. Parent bytes are durable before
	/// fee funding or broadcast, and every retry keeps those exact bytes.
	pub(crate) async fn process_expiry_payouts(&self, view: BlockRef) -> anyhow::Result<()> {
		let groups = self.db.read(async |t| t.pending_expiry_payouts().await).await?;
		for group in groups {
			let id = group.id;
			if let Err(error) = self.process_expiry_payout(view, group).await {
				warn!(allocation_id = id, error = %format_args!("{error:#}"), "Expiry payout remains pending");
			}
		}
		Ok(())
	}

	async fn process_expiry_payout(&self, view: BlockRef, group: PendingExpiryPayout) -> anyhow::Result<()> {
		let parent = match group.parent {
			Some(parent) => parent,
			None => self.build_expiry_payout(&group.inputs, &group.payouts).await?,
		};
		self.db.write(async |t| t.store_expiry_parent(view, group.id, &parent).await).await?;
		if matches!(bcd::tx_status(&self.bitcoind, parent.compute_txid()).await?, TxStatus::Confirmed(_)) {
			return Ok(());
		}
		let target = self.fee_estimator.regular();
		let (anchor, _) = parent.fee_anchor().context("expiry parent has no fee anchor")?;
		let active = bcd::get_mempool_spending_tx(&self.bitcoind, anchor).await?;
		let fees = if let Some(txid) = active {
			let (current_package_fee, weight) = mempool_child_fee(&self.bitcoind, txid).await?
				.context("expiry fee child left the mempool")?;
			if current_package_fee >= (parent.weight() + weight) * target {
				return Ok(());
			}
			MakeCpfpFees::Rbf { min_effective_fee_rate: target, current_package_fee }
		} else {
			MakeCpfpFees::Effective(target)
		};
		let mut wallet = self.watchman_wallet.lock().await;
		if !wallet.has_trusted_balance(self.config.min_cpfp_amount) {
			warn!(allocation_id = group.id, "Expiry payouts need confirmed usable watchman funds");
			return Ok(());
		}
		let child = wallet.make_signed_p2a_cpfp(&parent, fees)
			.context("funding expiry payout fee child")?;
		// As with upstream's progress CPFP, the child is rebuilt from current
		// wallet/Core state. Persist address allocation, not a child ledger.
		wallet.persist().await?;
		drop(wallet);
		crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
		self.db.write(async |t| {
			t.lock_watchman_tip(view).await?;
			t.query_one("SELECT id FROM expiry_allocation WHERE id=$1 FOR UPDATE", &[&group.id]).await?;
			ensure!(t.query_one("SELECT status FROM expiry_allocation_state WHERE id=$1", &[&group.id]).await?.get::<_, &str>(0) == "pending",
				"expiry allocation changed before package publication");
			let result = bcd::submit_package(&self.bitcoind, &[parent.clone(), child.clone()]).await?;
			bcd::require_package_acceptance(&result, &[&parent, &child])?;
			t.execute("UPDATE expiry_allocation SET broadcast_at=COALESCE(broadcast_at,NOW()) WHERE id=$1",
				&[&group.id]).await?;
			Ok(())
		}).await?;
		Ok(())
	}

}
