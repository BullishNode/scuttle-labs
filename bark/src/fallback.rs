//! Coin-specific payout destinations and authenticated fallback status.

mod refusal;
mod sync;
mod reconcile;
pub use refusal::FallbackRefusal;

use std::collections::BTreeMap;
use anyhow::Context;
use bitcoin::ScriptBuf;
use bitcoin::secp256k1::PublicKey;
use ark::{ProtocolEncoding, VtxoPolicy};
use server_rpc::protos;
use crate::Wallet;

pub(crate) fn next_key_index(last: Option<u32>) -> anyhow::Result<u32> {
	let next = last.map(|i| i.checked_add(1).context("vtxo key index exhausted"))
		.transpose()?.unwrap_or(0);
	ensure!(next < (1 << 31), "vtxo key index exhausted");
	Ok(next)
}

impl Wallet {

	/// Taproot destination of the exact mailbox identity checked at open.
	pub fn fallback_script(&self) -> ScriptBuf {
		ark::fallback::payout_script(self.mailbox_keypair().public_key())
	}

	/// Bind each actual arkoor output to its blinded mailbox point. External
	/// outputs use the recipient's supplied address; self outputs blind this
	/// wallet's fixed mailbox with the actual output keypair.
	pub(crate) async fn attach_payout_destinations(
		&self, package: &mut protos::ArkoorPackageCosignRequest,
		addresses: &BTreeMap<PublicKey, ark::Address>,
	) -> anyhow::Result<()> {
		let properties = self.properties().await?;
		if !properties.server_payouts.context("server payout capability is not known")? {
			return Ok(());
		}
		let server_mailbox_pubkey = match properties.server_mailbox_pubkey {
			Some(mailbox) => mailbox,
			None => self.require_ark_info().await?.mailbox_pubkey,
		};
		let policies = package.parts.iter()
			.flat_map(|part| part.outputs.iter().chain(&part.isolated_outputs))
			.map(|output| VtxoPolicy::deserialize(&output.policy))
			.collect::<Result<Vec<_>, _>>()?;
		let mut output_mailboxes = BTreeMap::new();
		for policy in policies {
			let key = policy.user_pubkey();
			let blinded = if let Some(address) = addresses.get(&key) {
				ensure!(address.policy() == &policy,
					"recipient address does not match arkoor output policy");
				match address.delivery() {
					[ark::address::VtxoDelivery::ServerMailbox { blinded_id }] => *blinded_id,
					_ => anyhow::bail!("recipient address requires one server mailbox delivery"),
				}
			} else {
				let (_, keypair) = self.pubkey_keypair(&key).await?
					.context(FallbackRefusal::RecipientMustUpdate)?;
				self.mailbox_identifier().to_blinded(server_mailbox_pubkey, &keypair)?
			};
			output_mailboxes.insert(policy.serialize(), blinded);
		}
		for output in package.parts.iter_mut()
			.flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs))
		{
			output.blinded_mailbox_id = output_mailboxes[&output.policy].as_ref().to_vec();
		}
		Ok(())
	}

}
