//! Refresh last observed display status without changing ordinary spent retirement.

use crate::Wallet;
use crate::vtxo::VtxoStateKind;

impl Wallet {
	/// Reconcile known commitments with best-effort authenticated status reads.
	/// Unanswered IDs retain their current state; success does not certify that
	/// every known commitment was refreshed.
	pub async fn sync_fallback_payouts(&self) -> anyhow::Result<()> {
		let tip = self.chain().tip().await?;
		let mut keyed = Vec::new();
		for coin in self.inner.db.get_all_vtxos().await? {
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
