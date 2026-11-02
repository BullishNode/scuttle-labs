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
	pub roots: Vec<ServerVtxo>,
	pub regular: Vec<BackingVtxoFact>,
}

const SNAPSHOT: &str = "
WITH RECURSIVE current_branch AS (
	SELECT vtxo_id,oor_spent_txid,spent_in_round,offboarded_in,policy_type FROM vtxo
	WHERE $2<>0 AND vtxo_id=ANY($1)
	UNION
	SELECT v.vtxo_id,v.oor_spent_txid,v.spent_in_round,v.offboarded_in,v.policy_type
	FROM vtxo v JOIN current_branch p ON v.vtxo_txid=p.oor_spent_txid
	WHERE ($2=2 OR (p.spent_in_round IS NULL AND p.offboarded_in IS NULL))
		AND ($2<>2 OR p.vtxo_id=ANY($1) OR p.policy_type NOT IN (
			'server-owned','expiry','checkpoint','hark-leaf-v1','hark-leaf',
			'server-htlc-receive-v1','server-htlc-receive'))
)
SELECT vtxo_id,anchor_point,id,vtxo,expiry,oor_spent_txid,spent_in_round,offboarded_in,
	banned_until_height,confirmed_height,spend_state::TEXT,created_at,updated_at,
	EXISTS(SELECT 1 FROM expiry_entitlement d WHERE d.vtxo_id=v.vtxo_id) AS decided_for_fallback,
	onchain_spent_height,onchain_spent_txid,lightning_htlc_subscription_id
FROM vtxo v WHERE ($2=0 AND anchor_point=ANY($1))
	OR ($2<>0 AND vtxo_id IN (SELECT vtxo_id FROM current_branch))
ORDER BY vtxo_id";

impl Tx<'_> {
	pub async fn backing_snapshot(&self, roots: &[ServerVtxo]) -> anyhow::Result<BackingSnapshot> {
		self.read_backing_snapshot(roots, 0).await
	}

	pub async fn backing_subtree_snapshot(&self, roots: &[ServerVtxo]) -> anyhow::Result<BackingSnapshot> {
		self.read_backing_snapshot(roots, 1).await
	}

	pub async fn backing_recovery_snapshot(&self, root: &ServerVtxo) -> anyhow::Result<BackingSnapshot> {
		self.read_backing_snapshot(std::slice::from_ref(root), 2).await
	}

	async fn read_backing_snapshot(&self, roots: &[ServerVtxo], scope: i32) -> anyhow::Result<BackingSnapshot> {
		let selected = roots.iter().map(|root| if scope == 0 { root.chain_anchor().to_string() } else { root.id().to_string() })
			.collect::<Vec<_>>();
		let rows = self.query(SNAPSHOT, &[&selected, &scope]).await?;
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
		Ok(BackingSnapshot { roots: roots.to_vec(), regular })
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
