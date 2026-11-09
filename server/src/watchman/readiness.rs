//! Allocate the current sweep frontier: analyse each group lock-free, then commit it in a short checked write.

use std::collections::BTreeMap;
use tracing::info;
use ark::{ServerVtxo, ServerVtxoPolicy};
use bitcoin_ext::BlockHeight;
use bitcoin_ext::BlockRef;
use crate::watchman::Watchman;

impl Watchman {
	pub(crate) async fn expiry_readiness_pass(&self, view: BlockRef, roots: Vec<ServerVtxo>) -> anyhow::Result<()> {
		crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
		let mut groups = BTreeMap::<BlockHeight, Vec<ServerVtxo>>::new();
		for root in roots {
			ensure!(root.server_pubkey() == self.signer.server_pubkey(), "expiry backing belongs to another server");
			// Exposed leaves follow their own claim/refund contract.
			if !matches!(root.policy(), ServerVtxoPolicy::User(_)) {
				groups.entry(root.expiry_height()).or_default().push(root);
			}
		}
		for roots in groups.into_values() {
			crate::database::expiry_chain::verify_expiry_tip(view, &self.bitcoind).await?;
			let group = self.db.read(async |tx| tx.plan_expiry_group(view, roots).await).await?;
			if let Some(group) = self.db.write(async |tx| tx.commit_expiry_group(view, group).await).await? {
				info!(allocation_id = group.id, inputs = group.inputs, dropped = group.dropped,
					"Expiry payout decision committed");
			}
		}
		Ok(())
	}
}
