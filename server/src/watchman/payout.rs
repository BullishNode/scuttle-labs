//! Pays expired coins to their owners. Fee bumps keep owner outputs unchanged.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Context;
use bitcoin::{Amount, FeeRate, Sequence, Transaction, Weight};
use bitcoin::psbt;
use bitcoin::sighash;
use bitcoin_ext::TxStatus;
use bitcoin_ext::TxOutExt;
use tracing::warn;

use crate::bitcoind as bcd;
use crate::wallet::BdkWalletExt;
use crate::wallet::PersistedWallet;
use bitcoin_ext::BlockRef;
use crate::database::expiry_payout::PendingExpiryPayout;

use ark::{ServerVtxoPolicy, VtxoPolicy};
use ark::vtxo::policy::signing::VtxoSigner;

use super::{saturating_add_feerates, Watchman};

impl Watchman {
	/// Sweep server coins and pay expired customer coins.
	pub(super) async fn process_expiry_sweeps(&self) -> anyhow::Result<()> {
		// Committed owner decisions are ready to publish without another
		// frontier scan or the more expensive ownership analysis below.
		self.process_expiry_payouts().await?;
		// Confiscated OffboardForfeit outputs are exclusively server-owned.
		// Their Bare encoding needs no customer genesis or payout allocation.
		// Use the original native claim: its miner fee comes from operator
		// collateral, not from a customer payout or a new wallet subsidy.
		let sweeps = self.scan_frontier().await?.sweeps;
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
		self.expiry_readiness_pass(sweeps).await?;
		// Expired customer backing never reaches upstream's drain-only builder.
		self.process_expiry_payouts().await?;
		Ok(())
	}

	/// Build one version-2 transaction with immutable owner outputs and an
	/// operator drain. BDK selects confirmed watchman inputs only if the Ark
	/// remainder cannot cover the mining fee in this same transaction.
	async fn build_direct_expiry_payout(
		&self, wallet: &mut PersistedWallet, group: &PendingExpiryPayout,
		view: BlockRef, target: FeeRate, prior: Option<&Transaction>,
	) -> anyhow::Result<(Transaction, Amount)> {
		ensure!(!group.inputs.is_empty(), "expiry payout has no backing inputs");
		ensure!(group.payouts.iter().all(|out| out.is_standard()
			&& out.script_pubkey != self.drain_spk),
			"expiry payout has a dust, nonstandard or operator-owned payee output");
		let roots = group.inputs.iter().map(|root| root.point()).collect::<BTreeSet<_>>();
		let prior_sponsors = prior.into_iter().flat_map(|tx| &tx.input)
			.filter(|input| !roots.contains(&input.previous_output))
			.map(|input| {
				let owned = wallet.list_output().find(|out| out.outpoint == input.previous_output)
					.context("prior payout fee input is not a watchman output")?;
				ensure!(owned.chain_position.is_confirmed(),
					"prior payout fee input was not confirmed");
				Ok((input.previous_output, owned.txout,
					Weight::from_wu(u64::try_from(bitcoin::consensus::serialize(&input.witness).len())?),
					input.sequence))
			}).collect::<anyhow::Result<Vec<_>>>()?;
		let unspendable = wallet.payout_unspendable_outputs();
		let mut clauses = Vec::with_capacity(group.inputs.len());
		let mut builder = wallet.build_tx();
		builder.version(2)
			.nlocktime(view.height.to_locktime().context("invalid payout lock height")?)
			.set_exact_sequence(Sequence::ZERO)
			.only_witness_utxo()
			.unspendable(unspendable)
			.drain_to(self.drain_spk.clone())
			.fee_rate(target);
		for root in &group.inputs {
			let clause = if *root.policy() == ServerVtxoPolicy::ServerOwned {
				None
			} else {
				Some(self.signer.find_signable_clause(root).await
					.with_context(|| format!("no expiry clause for {}", root.id()))?)
			};
			let sequence = clause.as_ref().and_then(|clause| clause.sequence())
				.unwrap_or(Sequence::ZERO);
			let witness_size = clause.as_ref().map(|clause| clause.witness_size(root))
				.unwrap_or(66);
			builder.add_foreign_utxo_with_sequence(root.point(), psbt::Input {
				witness_utxo: Some(root.txout()), ..Default::default()
			}, Weight::from_wu(u64::try_from(witness_size)?), sequence)
				.context("adding expiry backing to payout")?;
			clauses.push(clause);
		}
		for (outpoint, txout, weight, sequence) in prior_sponsors {
			builder.add_foreign_utxo_with_sequence(outpoint, psbt::Input {
				witness_utxo: Some(txout), ..Default::default()
			}, weight, sequence).context("reusing confirmed watchman fee input")?;
		}
		builder.set_recipients(group.payouts.iter()
			.map(|out| (out.script_pubkey.clone(), out.value)).collect());
		let mut psbt = builder.finish().context("funding direct expiry payout")?;
		wallet.sign_known_inputs(&mut psbt)?;
		let prevouts = psbt.inputs.iter().map(|input| input.witness_utxo.clone()
			.context("direct payout PSBT lacks a witness prevout"))
			.collect::<anyhow::Result<Vec<_>>>()?;
		let mut tx = psbt.unsigned_tx.clone();
		for (index, input) in tx.input.iter_mut().enumerate() {
			if !group.inputs.iter().any(|root| root.point() == input.previous_output) {
				input.witness = psbt.inputs[index].final_script_witness.clone()
					.context("watchman fee input was not finalized")?;
			}
		}
		let prevouts_ref = sighash::Prevouts::All(&prevouts);
		let mut cache = sighash::SighashCache::new(&mut tx);
		for (root, clause) in group.inputs.iter().zip(&clauses) {
			let index = cache.transaction().input.iter().position(|input|
				input.previous_output == root.point())
				.context("expiry backing disappeared from payout")?;
			let witness = if let Some(clause) = clause {
				self.signer.sign_input_with_clause(root, clause, index, &mut cache, &prevouts_ref).await?
			} else {
				self.signer.sign_input_with_keyspend(root, index, &mut cache, &prevouts_ref).await?
			};
			*cache.witness_mut(index).context("missing payout witness slot")? = witness;
		}
		ensure!(tx.version == bitcoin::transaction::Version::TWO,
			"direct expiry payout is not version 2");
		ensure!(tx.vsize() <= 100_000, "direct expiry payout exceeds standard size: {} vbytes", tx.vsize());
		ensure!(tx.input.iter().any(|input| input.sequence.to_consensus_u32() <= 0xFFFFFFFD),
			"direct expiry payout does not signal RBF");
		ensure!(tx.output.iter().all(TxOutExt::is_standard),
			"direct expiry payout contains a dust or nonstandard output");
		let expected = group.payouts.iter().map(|out| (out.script_pubkey.clone(), out.value))
			.collect::<BTreeMap<_, _>>();
		ensure!(expected.len() == group.payouts.len(), "duplicate payout script in allocation");
		let actual = tx.output.iter().filter(|out| expected.contains_key(&out.script_pubkey))
			.map(|out| (out.script_pubkey.clone(), out.value)).collect::<BTreeMap<_, _>>();
		ensure!(tx.output.iter().filter(|out| expected.contains_key(&out.script_pubkey)).count()
			== expected.len(), "direct payout duplicated an owner output");
		ensure!(actual == expected, "direct payout changed an owner output");
		ensure!(tx.output.iter().filter(|out| !expected.contains_key(&out.script_pubkey))
			.all(|out| out.script_pubkey == self.drain_spk),
			"direct payout contains a non-owner, non-operator output");
		let input_amount = prevouts.iter().try_fold(Amount::ZERO, |sum, out|
			sum.checked_add(out.value).context("direct payout input overflow"))?;
		let output_amount = tx.output.iter().try_fold(Amount::ZERO, |sum, out|
			sum.checked_add(out.value).context("direct payout output overflow"))?;
		let fee = input_amount.checked_sub(output_amount).context("direct payout overspends inputs")?;
		ensure!(fee >= target.fee_wu(tx.weight()).context("direct payout fee overflow")?,
			"direct payout fee falls below target");
		Ok((tx, fee))
	}

	/// Publish each fully resolved group in one fee-paying transaction.
	pub(crate) async fn process_expiry_payouts(&self) -> anyhow::Result<()> {
		let groups = self.db.read(async |t| t.pending_expiry_payouts().await).await?;
		for group in groups {
			// A block can arrive while an earlier payout is signed or funded.
			// Give each allocation its current common captaind/watchmand view.
			let Some(view) = self.db.read(async |t| t.expiry_view().await).await? else { break; };
			let id = group.id;
			if let Err(error) = self.process_expiry_payout(view, group).await {
				warn!(allocation_id = id, error = %format_args!("{error:#}"), "Expiry payout remains pending");
			}
		}
		Ok(())
	}

	async fn process_expiry_payout(&self, view: BlockRef, group: PendingExpiryPayout) -> anyhow::Result<()> {
		let target = self.fee_estimator.regular();
		let mut bump_target = target;
		let mut bump = false;
		if let Some(prior) = &group.parent {
			let status = bcd::tx_status(&self.bitcoind, prior.compute_txid()).await?;
			if matches!(status, TxStatus::Confirmed(_)) {
				self.db.write(async |t| {
					t.execute("UPDATE expiry_allocation SET broadcast_at=COALESCE(broadcast_at,NOW()) \
						WHERE id=$1 AND sweep_txid=$2",
						&[&group.id,&prior.compute_txid().to_string()]).await?;
					Ok(())
				}).await?;
				return Ok(());
			}
			let prior_fee = group.parent_fee.context("stored payout fee missing")?;
			let prior_rate = prior_fee / prior.weight();
			bump = prior_rate < target;
			if matches!(status, TxStatus::Mempool) && !bump {
				self.db.write(async |t| {
					t.execute("UPDATE expiry_allocation SET broadcast_at=COALESCE(broadcast_at,NOW()) \
						WHERE id=$1 AND sweep_txid=$2",
						&[&group.id,&prior.compute_txid().to_string()]).await?;
					Ok(())
				}).await?;
				return Ok(());
			}
			if bump {
				bump_target = target.max(saturating_add_feerates(
					prior_rate, self.config.incremental_relay_fee));
			}
		}
		crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
		let prepared = self.db.write(async |t| {
			t.lock_watchman_tip(view).await?;
			t.query_one("SELECT id FROM expiry_allocation WHERE id=$1 FOR UPDATE", &[&group.id]).await?;
			ensure!(t.query_one("SELECT status FROM expiry_allocation_state WHERE id=$1", &[&group.id]).await?.get::<_, &str>(0) == "pending",
				"expiry allocation changed before direct payout publication");
			// Wallet sync may hold the wallet while waiting for this connection.
			// Defer if busy; never wait for it while holding the publication tx.
			let Some(mut wallet) = self.watchman_wallet.try_lock() else { return Ok(None); };
			if let Some(parent) = &group.parent {
				// An attempt can be durable even if a crash prevented the later
				// wallet changeset from being saved. Restore its graph before RBF.
				for root in &group.inputs { wallet.insert_txout(root.point(), root.txout()); }
				wallet.commit_tx(parent);
			}
			let (payout, fee) = match (&group.parent, bump) {
				(Some(parent), true) => self.build_direct_expiry_payout(
					&mut wallet, &group, view, bump_target, Some(parent)).await?,
				(Some(parent), false) => (parent.clone(), group.parent_fee.context("stored payout fee missing")?),
				(None, _) => self.build_direct_expiry_payout(&mut wallet, &group, view, target, None).await?,
			};
			if group.parent.as_ref() != Some(&payout) {
				let checks = bcd::test_mempool_accept(&self.bitcoind, &[&payout]).await
					.context("preflighting direct expiry payout")?;
				ensure!(checks.len() == 1 && checks[0].allowed,
					"Core preflight rejected direct expiry payout: {:?}",
					checks.first().and_then(|check| check.reject_reason.as_deref()));
			}
			t.store_expiry_attempt(view, group.id, &payout, fee, &self.drain_spk).await?;
			for root in &group.inputs { wallet.insert_txout(root.point(), root.txout()); }
			wallet.commit_tx(&payout);
			if let Some(change) = wallet.staged() {
				t.store_changeset(wallet.kind(), change).await?;
			}
			Ok(Some((wallet, payout)))
		}).await?;
		if let Some((mut wallet, payout)) = prepared {
			// Keep staged changes on any database failure, as persist() does.
			let _ = wallet.take_staged();
			crate::telemetry::set_wallet_balance(wallet.kind(), wallet.balance());
			drop(wallet);
			crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
			if !matches!(bcd::tx_status(&self.bitcoind, payout.compute_txid()).await?, TxStatus::Confirmed(_)) {
				bcd::broadcast_tx(&self.bitcoind, &payout).await?;
			}
			self.db.write(async |t| {
				t.execute("UPDATE expiry_allocation SET broadcast_at=COALESCE(broadcast_at,NOW()) WHERE id=$1",
					&[&group.id]).await?;
				Ok(())
			}).await?;
		}
		Ok(())
	}

}
