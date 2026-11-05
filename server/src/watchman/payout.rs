//! Pays expired coins to their owners. Fee bumps keep the payout transaction unchanged.

use std::collections::HashMap;

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
use crate::wallet::BdkWalletExt;
use bitcoin_ext::BlockRef;
use crate::database::expiry_payout::{MAX_PAYOUT_VSIZE, PendingExpiryPayout};

use ark::{ServerVtxo, ServerVtxoPolicy, VtxoPolicy};

use super::{saturating_add_feerates, Watchman};

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
	/// Sweep server coins, pay expired customer coins, and advance pending splits.
	pub(super) async fn process_expiry_sweeps(&self) -> anyhow::Result<()> {
		// Confiscated OffboardForfeit outputs are exclusively server-owned.
		// Their Bare encoding needs no customer genesis or payout allocation.
		// Use the original native claim: its miner fee comes from operator
		// collateral, not from a customer payout or a new wallet subsidy.
		let sweeps = self.scan_frontier().await.sweeps;
		let ids = sweeps.iter().filter(|v| matches!(v.policy(), ServerVtxoPolicy::ServerOwned
			| ServerVtxoPolicy::User(VtxoPolicy::Pubkey(_))))
			.map(|v| v.id().to_string()).collect::<Vec<_>>();
		let eligible = self.db.read(async |t| Ok(t.query(
			"SELECT vtxo_id,spend_state::TEXT FROM vtxo WHERE vtxo_id=ANY($1) AND spend_state IN ('offboard-forfeit','pool')",
			&[&ids]).await?.into_iter().map(|r| (r.get::<_, String>(0),r.get::<_, String>(1))).collect::<HashMap<_, _>>()
		)).await?;
		let mut claims = Vec::new();
		for vtxo in &sweeps {
			let state = eligible.get(&vtxo.id().to_string()).map(String::as_str);
			let owned = match (vtxo.policy(), state) {
				(ServerVtxoPolicy::ServerOwned, Some("offboard-forfeit")) => true,
				(ServerVtxoPolicy::User(VtxoPolicy::Pubkey(policy)), Some("pool")) => {
					policy.user_pubkey == vtxo.server_pubkey() || self.db.read(async |t|
						t.fetch_ephemeral_tweak(policy.user_pubkey).await
					).await?.is_some()
				},
				_ => false,
			};
			if owned {
				self.update_mempool_spend(vtxo.id()).await;
				claims.push(vtxo.clone());
			}
		}
		if !claims.is_empty() { self.process_claims(claims).await?; }
		// Operator-only claims above do not depend on customer view readiness.
		let Some(view) = self.db.read(async |t| t.expiry_view().await).await? else { return Ok(()); };
		crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
		self.expiry_readiness_pass(view, sweeps).await?;
		// Expired customer backing never reaches upstream's drain-only builder.
		self.process_expiry_payouts(view).await?;
		let splits = self.db.read(async |t| t.pending_expiry_splits().await).await?;
		for (txid, root) in splits {
			crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
			let ids = [root.id()];
			let mut fee_rate = self.fee_estimator.regular();
			self.update_mempool_spend(root.id()).await;
			if let Some(spend) = self.get_mempool_spend(root.id()) {
				if spend.txid != txid {
					warn!(vtxo_id = %root.id(), expected = %txid, actual = %spend.txid,
						"Expiry progress is pinned by a different spender; retaining its hold");
					continue;
				}
				if spend.fee_rate >= fee_rate { continue; }
				fee_rate = fee_rate.max(saturating_add_feerates(
					spend.fee_rate, self.config.incremental_relay_fee
				));
			}
			let committed_wallet = self.db.write(async |t| {
				t.lock_watchman_tip(view).await?;
				// Wallet sync may hold the wallet while waiting for this connection.
				let Some(mut wallet) = self.watchman_wallet.try_lock() else { return Ok(None); };
				if !wallet.has_trusted_balance(self.config.min_cpfp_amount) {
					warn!(vtxo_id = %root.id(), "Expiry splitting needs confirmed usable watchman funds");
					return Ok(None);
				}
				// A request may outlive a later refresh, offboard or key drop.
				// Reprove preservation and retain its sorted path locks through
				// publication, rather than trusting the old request's txid alone.
				if !t.request_expiry_split(view, &root).await
					.context("expiry split request revalidation")? { return Ok(None); }
				let current = t.query_one("
					SELECT progress_txid FROM expiry_split_request
					WHERE vtxo_id=$1
				", &[&root.id().to_string()]).await.context("expiry split instruction lookup")?;
				ensure!(
					current.get::<_, &str>("progress_txid") == txid.to_string(),
					"expiry split instruction changed"
				);
				t.lock_unallocated_watchman_inputs(&ids).await.context("expiry split input locks")?;
				if self.process_progress_tx(t, &mut wallet, txid, &root, fee_rate).await
					.context("expiry split progress publication")? {
					Ok(Some(wallet))
				} else { Ok(None) }
			}).await;
			match committed_wallet {
				Ok(Some(mut wallet)) => {
					let _ = wallet.take_staged();
					crate::telemetry::set_wallet_balance(wallet.kind(), wallet.balance());
				},
				Ok(None) => {},
				Err(error) => warn!(vtxo_id = %ids[0], progress_txid = %txid, error = %format!("{error:#}"),
					"Expiry split remains pending"),
			}
		}
		Ok(())
	}

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
		crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
		let committed_wallet = self.db.write(async |t| {
			t.lock_watchman_tip(view).await?;
			t.query_one("SELECT id FROM expiry_allocation WHERE id=$1 FOR UPDATE", &[&group.id]).await?;
			ensure!(t.query_one("SELECT status FROM expiry_allocation_state WHERE id=$1", &[&group.id]).await?.get::<_, &str>(0) == "pending",
				"expiry allocation changed before package publication");
			// Wallet sync may hold the wallet while waiting for this connection.
			// Defer if busy; never wait for it while holding the publication tx.
			let Some(mut wallet) = self.watchman_wallet.try_lock() else { return Ok(None); };
			if !wallet.has_trusted_balance(self.config.min_cpfp_amount) {
				warn!(allocation_id = group.id, "Expiry payouts need confirmed usable watchman funds");
				return Ok(None);
			}
			let child = wallet.make_signed_p2a_cpfp(&parent, fees)
				.context("funding expiry payout fee child")?;
			let result = bcd::submit_package(&self.bitcoind, &[parent.clone(), child.clone()]).await?;
			bcd::require_package_acceptance(&result, &[&parent, &child])?;
			wallet.commit_tx(&child);
			if let Some(change) = wallet.staged() {
				t.store_changeset(wallet.kind(), change).await?;
			}
			t.execute("UPDATE expiry_allocation SET broadcast_at=COALESCE(broadcast_at,NOW()) WHERE id=$1",
				&[&group.id]).await?;
			Ok(Some(wallet))
		}).await?;
		if let Some(mut wallet) = committed_wallet {
			// Keep staged changes on any database failure, as persist() does.
			let _ = wallet.take_staged();
			crate::telemetry::set_wallet_balance(wallet.kind(), wallet.balance());
		}
		Ok(())
	}

}
