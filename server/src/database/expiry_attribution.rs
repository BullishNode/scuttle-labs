//! Authenticated payout mailbox links.

use std::collections::BTreeMap;
use std::str::FromStr;

use bitcoin::secp256k1::{Keypair, PublicKey};
use bitcoin::{Network, ScriptBuf};

use ark::fallback::OwnerLink;
use ark::VtxoId;

use crate::error::OwnershipRefusal;
use super::expiry_ownership::{OwnershipDisposition, OwnershipProposal};
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

	/// Resolve the stage-selected key from admission's immutable binding.
	/// Birth metadata constrains an ordinary coin, not a different entitled key
	/// selected by an input-owner, receive-claim or participation rule.
	pub async fn derived_expiry_owner(&self, ownership: &OwnershipProposal) -> anyhow::Result<Attribution> {
		let OwnershipDisposition::Customer { user_pubkey } = &ownership.disposition
			else { return Ok(Attribution::Unresolved); };
		if ownership.evidence.is_none() { return Ok(Attribution::Unresolved); }
		self.expiry_owner(*user_pubkey).await
	}

	/// Verify both holders once at admission and keep the first signed binding.
	pub async fn verify_owner_link(
		&self, server_mailbox_key: &Keypair, user_pubkey: PublicKey,
		link: &OwnerLink, network: Network,
	) -> anyhow::Result<PublicKey> {
		let mailbox = link.verify_joint(network, server_mailbox_key, user_pubkey)
			.map_err(|_| OwnershipRefusal::Invalid(user_pubkey))?.as_pubkey();
		self.execute("INSERT INTO owner_key (user_pubkey, mailbox_pk, blinded_id)
			VALUES ($1, $2, $3) ON CONFLICT (user_pubkey) DO NOTHING",
			&[&user_pubkey.to_string(), &mailbox.to_string(), &link.blinded_id.to_string()]).await?;
		let canonical = self.query_one("SELECT mailbox_pk, blinded_id FROM owner_key WHERE user_pubkey=$1",
			&[&user_pubkey.to_string()]).await?;
		if canonical.get::<_, &str>("mailbox_pk") != mailbox.to_string()
			|| canonical.get::<_, &str>("blinded_id") != link.blinded_id.to_string()
		{
			return Err(OwnershipRefusal::Conflicting(user_pubkey).into());
		}
		Ok(mailbox)
	}

	/// Read admission's immutable mailbox. No signature is replayed at payout.
	pub async fn expiry_owner(&self, user_pubkey: PublicKey) -> anyhow::Result<Attribution> {
		let Some(row) = self.query_opt("SELECT mailbox_pk FROM owner_key WHERE user_pubkey=$1",
			&[&user_pubkey.to_string()]).await? else { return Ok(Attribution::Unresolved); };
		let Ok(mailbox) = PublicKey::from_str(row.get("mailbox_pk"))
			else { return Ok(Attribution::Unresolved); };
		Ok(Attribution::Owned { mailbox_pk: mailbox,
			spk: ark::fallback::payout_script(mailbox) })
	}
}

#[cfg(test)]
mod tests {
	use ark::{ProtocolEncoding, ServerVtxoPolicy, VtxoPolicy};
	use bitcoin::hex::DisplayHex;

	/// The customer-birth trigger repeats the round padding policy as a byte
	/// literal. Fail here, not at the first padded round, if the key changes.
	#[test]
	fn customer_coin_owner_trigger_exempts_the_round_padding_policy() {
		let policy = ServerVtxoPolicy::User(VtxoPolicy::new_pubkey(*crate::round::UNSPENDABLE));
		let literal = format!("decode('{}', 'hex')", policy.serialize().as_hex());
		assert!(include_str!("migrations/V78__expiry_payouts.sql").contains(&literal), "{literal}");
	}
}
