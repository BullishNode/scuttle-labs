//! Current registered branches for selected watchman frontier outputs.

use std::str::FromStr;
use anyhow::Context;
use bitcoin::Txid;
use bitcoin_ext::BlockHeight;
use tokio_postgres::Row;
use ark::{ProtocolDecodingError, ProtocolEncoding, ServerVtxo, ServerVtxoPolicy};
use ark::vtxo::{Bare, Full, VtxoValidationError};
use super::{Tx, VtxoState};

#[derive(Clone)]
pub struct BackingVtxoFact {
	pub state: VtxoState<Bare, ServerVtxoPolicy>,
	/// Present only when the stored encoding supplies a complete path.
	/// Availability still does not prove signatures or physical containment.
	pub full_vtxo: Option<ServerVtxo<Full>>,
	pub onchain_spent_height: Option<BlockHeight>,
	pub onchain_spent_txid: Option<Txid>,
	pub lightning_htlc_subscription_id: Option<i64>,
}


pub struct BackingSnapshot {
	pub regular: Vec<BackingVtxoFact>,
}

const SNAPSHOT: &str = "
WITH RECURSIVE current_branch AS (
	SELECT vtxo_id,oor_spent_txid,spent_in_round,offboarded_in FROM vtxo
	WHERE vtxo_id=ANY($1)
	UNION
	SELECT v.vtxo_id,v.oor_spent_txid,v.spent_in_round,v.offboarded_in
	FROM vtxo v JOIN current_branch p ON v.vtxo_txid=p.oor_spent_txid
	WHERE p.spent_in_round IS NULL AND p.offboarded_in IS NULL
)
SELECT vtxo_id,anchor_point,id,vtxo,expiry,exit_delta,policy,server_pubkey,amount,
	oor_spent_txid,spent_in_round,offboarded_in,
	banned_until_height,confirmed_height,spend_state::TEXT,created_at,updated_at,
	EXISTS(SELECT 1 FROM expiry_entitlement d WHERE d.vtxo_id=v.vtxo_id) AS decided_for_fallback,
	onchain_spent_height,onchain_spent_txid,lightning_htlc_subscription_id
FROM vtxo v WHERE vtxo_id IN (SELECT vtxo_id FROM current_branch)
ORDER BY vtxo_id";

impl Tx<'_> {
	pub async fn backing_subtree_snapshot(&self, roots: &[ServerVtxo]) -> anyhow::Result<BackingSnapshot> {
		let selected = roots.iter().map(|root| root.id().to_string()).collect::<Vec<_>>();
		let rows = self.query(SNAPSHOT, &[&selected]).await?;
		let mut regular = Vec::with_capacity(rows.len());
		for row in rows {
			let onchain_spent_height = optional_height(&row, "onchain_spent_height")?;
			let onchain_spent_txid = row.get::<_, Option<&str>>("onchain_spent_txid").map(Txid::from_str).transpose()?;
			let lightning_htlc_subscription_id = row.get("lightning_htlc_subscription_id");
			let full_vtxo = decode_genesis(row.get("vtxo"))?;
			let state = VtxoState::<Bare, ServerVtxoPolicy>::try_from(row)?;
			regular.push(BackingVtxoFact { state, full_vtxo, onchain_spent_height, onchain_spent_txid,
				lightning_htlc_subscription_id });
		}
		Ok(BackingSnapshot { regular })
	}
}

fn decode_genesis(bytes: &[u8]) -> anyhow::Result<Option<ServerVtxo<Full>>> {
	match ServerVtxo::<Full>::deserialize(bytes) {
		Ok(full) => Ok(Some(full)),
		Err(ProtocolDecodingError::Invalid { source: Some(source), .. })
			if matches!(source.downcast_ref::<VtxoValidationError>(), Some(VtxoValidationError::MissingGenesisItems)) =>
		{
			Ok(None)
		},
		Err(error) => Err(error.into()),
	}
}

fn optional_height(row: &Row, column: &str) -> anyhow::Result<Option<BlockHeight>> {
	row.get::<_, Option<i32>>(column).map(BlockHeight::try_from).transpose().context("invalid snapshot chain height")
}
