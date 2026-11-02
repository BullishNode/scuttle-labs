//! Allocate the current sweep frontier under the existing decision locks.

use std::collections::BTreeMap;
use tracing::{info, warn};
use ark::{ServerVtxo, ServerVtxoPolicy};
use bitcoin_ext::BlockHeight;
use crate::database::Tx;
use bitcoin_ext::BlockRef;
use crate::watchman::Watchman;

fn database_failure(error: &anyhow::Error) -> bool {
	 error.downcast_ref::<tokio_postgres::Error>().is_some()
		|| error.downcast_ref::<bb8::RunError<tokio_postgres::Error>>().is_some()
}

impl Tx<'_> {
	pub(crate) async fn request_held_expiry_split(
		&self, view: BlockRef, root: &ServerVtxo, reason: &str,
	) -> anyhow::Result<()> {
		self.observe_expiry_hold(root.id(), reason);
		match self.request_expiry_split(view, root).await {
			Ok(true) => self.observe_expiry_hold(root.id(), "output_requires_split"),
			Ok(false) => {},
			Err(error) => {
				if database_failure(&error) { return Err(error); }
				warn!(vtxo_id = %root.id(), error = %format!("{error:#}"),
					"Signed expiry split remains unavailable; other outputs continue");
			},
		}
		Ok(())
	}
}

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
			if let Some(group) = self.db.write(async |tx| tx.commit_expiry_group(view, roots).await).await? {
				info!(allocation_id = group.id, inputs = group.inputs, dropped = group.dropped,
					"Expiry payout decision committed");
			}
		}
		Ok(())
	}
}
