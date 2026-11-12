//! Immutable sources for expiry payout mailboxes.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use bitcoin::secp256k1::PublicKey;

use ark::{ServerVtxoPolicy, VtxoId};

use super::Tx;

impl Tx<'_> {
	/// Immutable birth ownership only; recovery hints do not populate this map.
	pub async fn coin_owner_mailboxes(&self, ids: &[VtxoId]) -> anyhow::Result<BTreeMap<VtxoId, PublicKey>> {
		let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
		self.query("SELECT vtxo_id,mailbox_pk FROM coin_owner WHERE vtxo_id=ANY($1)", &[&ids]).await?
			.into_iter().map(|row| Ok((row.get::<_, &str>(0).parse()?,
				row.get::<_, &str>(1).parse()?))).collect()
	}

	/// The mailbox of the user coins that funded an unregistered output,
	/// found by following registered out-of-round spends. Every funding
	/// mailbox must agree.
	pub(crate) async fn expiry_input_owner(&self, id: VtxoId) -> anyhow::Result<Option<PublicKey>> {
		let mut pending = vec![id];
		let mut seen = BTreeSet::from([id]);
		let mut mailboxes = BTreeSet::new();
		while let Some(current_id) = pending.pop() {
			let current = self.get_server_vtxo_by_id(current_id).await?.vtxo;
			let Some(last) = current.transactions().last() else { return Ok(None); };
			if last.tx.input.is_empty() { return Ok(None); }
			for input in &last.tx.input {
				let parent = VtxoId::from(input.previous_output);
				if !seen.insert(parent) { return Ok(None); }
				let Some(row) = self.query_opt("SELECT v.vtxo_id,c.mailbox_pk FROM vtxo v \
					LEFT JOIN coin_owner c ON c.vtxo_id=v.vtxo_id \
					WHERE v.vtxo_id=$1 AND v.oor_spent_txid=$2",
					&[&parent.to_string(), &current.point().txid.to_string()]).await? else { return Ok(None); };
				let source = self.get_server_vtxo_by_id(parent).await?.vtxo;
				match source.policy() {
					ServerVtxoPolicy::Checkpoint(_) => pending.push(parent),
					ServerVtxoPolicy::User(_) => {
						let Some(mailbox) = row.get::<_, Option<&str>>("mailbox_pk") else { return Ok(None); };
						mailboxes.insert(PublicKey::from_str(mailbox)?);
					},
					_ => return Ok(None),
				}
			}
		}
		if mailboxes.len() != 1 { return Ok(None); }
		Ok(mailboxes.pop_first())
	}
}
