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

	/// Bind each actual arkoor output to a full address. An externally supplied
	/// recipient address stays exact; self outputs use their actual policy and
	/// existing output keypair with the wallet's fixed mailbox.
	pub(crate) async fn attach_payout_destinations(
		&self, package: &mut protos::ArkoorPackageCosignRequest,
		addresses: &BTreeMap<PublicKey, ark::Address>,
	) -> anyhow::Result<()> {
		let properties = self.properties().await?;
		if !properties.server_payouts.context("server payout capability is not known")? {
			return Ok(());
		}
		let (server_pubkey, server_mailbox_pubkey) = match
			(properties.server_pubkey, properties.server_mailbox_pubkey) {
				(Some(server), Some(mailbox)) => (server, mailbox),
				_ => {
					let info = self.require_ark_info().await?;
					(info.server_pubkey, info.mailbox_pubkey)
				},
			};
		let policies = package.parts.iter()
			.flat_map(|part| part.outputs.iter().chain(&part.isolated_outputs))
			.map(|output| VtxoPolicy::deserialize(&output.policy))
			.collect::<Result<Vec<_>, _>>()?;
		let mut output_addresses = BTreeMap::new();
		for policy in policies {
			let key = policy.user_pubkey();
			if let Some(address) = addresses.get(&key) {
				ensure!(address.policy() == &policy,
					"recipient address does not match arkoor output policy");
				output_addresses.insert(policy.serialize(), address.clone());
			} else {
				let (_, keypair) = self.pubkey_keypair(&key).await?
					.context(FallbackRefusal::RecipientMustUpdate)?;
				let address = ark::Address::builder()
					.testnet(properties.network != bitcoin::Network::Bitcoin)
					.server_pubkey(server_pubkey)
					.policy(policy.clone())
					.mailbox(server_mailbox_pubkey, self.mailbox_identifier(), &keypair)?
					.into_address()?;
				output_addresses.insert(policy.serialize(), address);
			}
		}
		for output in package.parts.iter_mut()
			.flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs))
		{
			let address = &output_addresses[&output.policy];
			output.address = Some(address.to_string());
		}
		Ok(())
	}

}
