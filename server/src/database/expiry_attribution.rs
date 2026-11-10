//! Immutable sources for expiry payout mailboxes.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use bitcoin::secp256k1::PublicKey;
use bitcoin::ScriptBuf;

use ark::{ServerVtxoPolicy, VtxoId};

use super::expiry_ownership::{OwnershipBindingSource, OwnershipDisposition, OwnershipProposal};
use super::Tx;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attribution {
	Owned { mailbox_pk: PublicKey, spk: ScriptBuf },
	Unresolved,
}

impl Tx<'_> {
	/// Immutable birth ownership only; recovery hints do not populate this map.
	pub async fn coin_owner_mailboxes(&self, ids: &[VtxoId]) -> anyhow::Result<BTreeMap<VtxoId, PublicKey>> {
		let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
		self.query("SELECT vtxo_id,mailbox_pk FROM coin_owner WHERE vtxo_id=ANY($1)", &[&ids]).await?
			.into_iter().map(|row| Ok((row.get::<_, &str>(0).parse()?,
				row.get::<_, &str>(1).parse()?))).collect()
	}

	/// Follow registered out-of-round spends to each user coin funding an
	/// unregistered output. Every funding mailbox must agree.
	pub(crate) async fn expiry_input_owner_sources(&self, id: VtxoId)
		-> anyhow::Result<Option<Vec<(VtxoId, PublicKey, PublicKey)>>>
	{
		let mut pending = vec![id];
		let mut seen = BTreeSet::from([id]);
		let mut sources = Vec::new();
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
					ServerVtxoPolicy::User(policy) => {
						let Some(mailbox) = row.get::<_, Option<&str>>("mailbox_pk") else { return Ok(None); };
						sources.push((parent, policy.user_pubkey(), PublicKey::from_str(mailbox)?));
					},
					_ => return Ok(None),
				}
			}
		}
		sources.sort_by_key(|source| source.0);
		if sources.is_empty() || sources.iter().any(|source| source.2 != sources[0].2) { return Ok(None); }
		Ok(Some(sources))
	}

	/// Resolve the stage-selected obligation from its immutable source record.
	pub async fn derived_expiry_owner(&self, ownership: &OwnershipProposal) -> anyhow::Result<Attribution> {
		let OwnershipDisposition::Customer { user_pubkey } = &ownership.disposition
			else { return Ok(Attribution::Unresolved); };
		if ownership.evidence.is_none() { return Ok(Attribution::Unresolved); }
		let mailbox = match &ownership.binding_source {
			OwnershipBindingSource::Coin(id) => {
				if *id != ownership.id { return Ok(Attribution::Unresolved); }
				let coin = self.get_server_vtxo_by_id(*id).await?.vtxo;
				if !matches!(coin.policy(), ServerVtxoPolicy::User(policy) if policy.user_pubkey() == *user_pubkey) {
					return Ok(Attribution::Unresolved);
				}
				self.coin_owner_mailboxes(&[*id]).await?.get(id).copied()
			},
			OwnershipBindingSource::InputCoins(ids) => {
				let Some(sources) = self.expiry_input_owner_sources(ownership.id).await? else { return Ok(Attribution::Unresolved); };
				let actual = sources.iter().map(|source| source.0).collect::<Vec<_>>();
				if &actual != ids || !sources.iter().any(|source| source.1 == *user_pubkey) {
					return Ok(Attribution::Unresolved);
				}
				Some(sources[0].2)
			},
			OwnershipBindingSource::Subscription(id) => {
				match self.receive_claim_recipient(ownership.id).await? {
					Some((_, key, mailbox, subscription)) if key == *user_pubkey && subscription == *id => Some(mailbox),
					_ => None,
				}
			},
			OwnershipBindingSource::Allocation(id) => {
				let row = self.query_opt("SELECT c.owner_mailbox,c.user_pubkey FROM expiry_allocation_coin c \
					JOIN expiry_entitlement e ON e.id=c.entitlement_id \
					WHERE c.allocation_id=$1 AND e.vtxo_id=$2 AND c.disposition='customer'",
					&[id, &ownership.id.to_string()]).await?;
				row.and_then(|row| {
					if row.get::<_, Option<&str>>("user_pubkey") != Some(user_pubkey.to_string().as_str()) { return None; }
					row.get::<_, Option<&str>>("owner_mailbox").and_then(|s| PublicKey::from_str(s).ok())
				})
			},
		};
		Ok(match mailbox {
			Some(mailbox_pk) => Attribution::Owned { mailbox_pk,
				spk: ark::fallback::payout_script(mailbox_pk) },
			None => Attribution::Unresolved,
		})
	}
}
