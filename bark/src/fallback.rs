//! Coin-specific payout destinations and authenticated fallback status.

mod refusal;
mod sync;
mod reconcile;
pub use refusal::FallbackRefusal;

use std::collections::{BTreeMap, BTreeSet};
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

	/// Bind each actual arkoor output to its recipient address or this wallet's
	/// fixed mailbox. The server verifies these values against the signed output.
	pub(crate) async fn attach_payout_destinations(
		&self, package: &mut protos::ArkoorPackageCosignRequest,
		addresses: &BTreeMap<PublicKey, ark::Address>,
	) -> anyhow::Result<()> {
		if !self.properties().await?.server_payouts.context("server payout capability is not known")? {
			return Ok(());
		}
		let mailbox = self.mailbox_identifier().as_pubkey().serialize().to_vec();
		let keys = package.parts.iter()
			.flat_map(|part| part.outputs.iter().chain(&part.isolated_outputs))
			.map(|output| Ok(VtxoPolicy::deserialize(&output.policy)?.user_pubkey()))
			.collect::<anyhow::Result<BTreeSet<_>>>()?;
		for key in keys {
			if let Some(address) = addresses.get(&key) {
				ensure!(address.policy().user_pubkey() == key,
					"recipient address does not match arkoor output key");
			} else {
				ensure!(self.pubkey_keypair(&key).await?.is_some(),
					FallbackRefusal::RecipientMustUpdate);
			}
		}
		for output in package.parts.iter_mut()
			.flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs))
		{
			let key = VtxoPolicy::deserialize(&output.policy)?.user_pubkey();
			if let Some(address) = addresses.get(&key) {
				output.address = Some(address.to_string());
			} else {
				output.mailbox_id = Some(mailbox.clone());
			}
		}
		Ok(())
	}

}
