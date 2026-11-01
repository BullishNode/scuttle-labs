//! Revisit known commitments, including terminal coins after reopen or reorg.

use std::collections::HashSet;
use crate::Wallet;
use crate::actions::WalletActionCheckpoint;
use crate::vtxo::VtxoStateKind;

impl Wallet {
	/// Reconcile known commitments with best-effort authenticated status reads.
	/// Unanswered IDs retain their current state; success does not certify that
	/// every known commitment was refreshed.
	pub async fn sync_fallback_payouts(&self) -> anyhow::Result<()> {
		let mut operation_sources = HashSet::new();
		for checkpoint in self.inner.db.get_all_wallet_action_checkpoints().await? {
			if let WalletActionCheckpoint::LightningReceive(recv) = checkpoint {
				if recv.fallback_status.is_some() { operation_sources.extend(recv.grant_ids); }
			}
		}
		for recv in self.inner.db.fallback_lightning_receives().await? {
			let claim = recv.association.as_ref().ok_or_else(|| anyhow::anyhow!("receive association disappeared"))?;
			let _history_guard = self.lock_fallback_history(std::time::Duration::from_secs(30)).await?;
			let status = self.receive_fallback_status(recv.payment_hash, &recv.invoice, &claim.grant_ids).await?
				.ok_or_else(|| anyhow::anyhow!("missing receive outcome cannot release a known commitment"))?;
			self.apply_receive_fallback(recv.payment_hash, &claim.grant_ids, status).await?;
			self.inner.db.set_receive_fallback_status(recv.payment_hash, status).await?;
			operation_sources.extend(claim.grant_ids.iter().copied());
		}
		let tip = self.chain().tip().await?;
		let mut keyed = Vec::new();
		for coin in self.inner.db.get_all_vtxos().await? {
			if operation_sources.contains(&coin.id()) { continue; }
			if coin.fallback_status.is_some() || (coin.expiry_height() <= tip
				&& VtxoStateKind::UNSPENT_STATES.contains(&coin.state.kind())) {
				keyed.push((coin.id(), self.get_vtxo_key(&coin.vtxo).await?));
			}
		}
		let queries = keyed.iter().map(|(id,key)| (*id,key)).collect::<Vec<_>>();
		self.fetch_vtxo_spend_states(&queries).await?;
		Ok(())
	}
}
