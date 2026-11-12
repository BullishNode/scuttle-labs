//! Decide each expired frontier root: analyse it lock-free, then commit it in a short checked write.

use tracing::{info, warn};
use ark::{ServerVtxo, ServerVtxoPolicy};
use crate::watchman::Watchman;

impl Watchman {
	/// An error on one root holds only that root; the others are still decided.
	pub(crate) async fn expiry_readiness_pass(&self, mut roots: Vec<ServerVtxo>) {
		// Exposed leaves follow their own claim/refund contract.
		roots.retain(|root| !matches!(root.policy(), ServerVtxoPolicy::User(_)));
		roots.sort_by_key(|root| root.expiry_height());
		for root in roots {
			let id = root.id();
			if let Err(error) = self.decide_expiry_root(root).await {
				warn!(root = %id, error = %format_args!("{error:#}"), "Expiry payout decision failed");
			}
		}
	}

	/// One root is one allocation and one payout transaction. Each root
	/// reads the current view, so a new tip cannot invalidate a long pass.
	async fn decide_expiry_root(&self, root: ServerVtxo) -> anyhow::Result<()> {
		let Some(view) = self.db.read(async |tx| tx.expiry_view().await).await? else {
			return Ok(());
		};
		crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
		let Some(plan) = self.db.read(async |tx| tx.plan_expiry_root(view, root.clone()).await).await? else {
			return Ok(());
		};
		let id = self.db.write(async |tx| tx.commit_expiry_root(view, plan).await).await?;
		info!(allocation_id = id, root = %root.id(), "Expiry payout decision committed");
		Ok(())
	}
}
