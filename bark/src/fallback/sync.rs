//! Refresh last observed display status without changing ordinary spent retirement.

use crate::Wallet;
use crate::vtxo::{VtxoLockHolder, VtxoState, VtxoStateKind};
use ark::vtxo::VtxoRef;
use server_rpc::protos::VtxoSpendState;

impl Wallet {
	/// A completed round may be stored while status RPC is unavailable. Keep
	/// its output locked until the server attests this wallet's mailbox.
	pub(crate) async fn release_verified_round_output(&self, output: impl VtxoRef + Copy) -> anyhow::Result<()> {
		let key = self.get_vtxo_key(output).await?;
		let (state, committed) = self.fetch_vtxo_status(output.vtxo_id(), &key).await?;
		ensure!(state == VtxoSpendState::Spendable && !committed,
			"round output is not attested spendable for this mailbox");
		self.unlock_vtxos([output.vtxo_id()], Some(VtxoLockHolder::RoundPayoutCheck)).await
	}

	/// Reconcile known commitments with best-effort authenticated status reads.
	/// Unanswered IDs retain their current state; success does not certify that
	/// every known commitment was refreshed.
	pub async fn sync_fallback_payouts(&self) -> anyhow::Result<()> {
		if self.properties().await?.server_payouts == Some(false) { return Ok(()); }
		let tip = self.chain().tip().await?;
		let mut keyed = Vec::new();
		for coin in self.inner.db.get_all_vtxos().await? {
			if coin.state == (VtxoState::Locked { holder: Some(VtxoLockHolder::RoundPayoutCheck) }) {
				if let Err(error) = self.release_verified_round_output(&coin.vtxo).await {
					log::warn!("Round output {} remains locked pending payout-owner verification: {error:#}", coin.id());
				}
			}
			if coin.fallback_status.is_some() || (coin.expiry_height() <= tip
				&& VtxoStateKind::UNSPENT_STATES.contains(&coin.state.kind())) {
				keyed.push((coin.id(), self.get_vtxo_key(&coin.vtxo).await?));
			}
		}
		let queries = keyed.iter().map(|(id,key)| (*id,key)).collect::<Vec<_>>();
		self.fetch_vtxo_statuses(&queries).await?;
		Ok(())
	}
}
