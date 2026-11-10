//! Bind request destinations to the exact VTXOs created by a cosignature.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context};
use bitcoin::Network;
use bitcoin::secp256k1::{Keypair, PublicKey};

use ark::{ProtocolEncoding, VtxoId, VtxoPolicy};
use ark::arkoor::package::ArkoorPackageBuilder;
use ark::arkoor::state::ServerCanCosign;
use ark::mailbox::MailboxIdentifier;
use server_rpc::protos;

/// Request-scoped destination map. It is never a durable lookup by coin key.
#[derive(Default)]
pub(crate) struct DestinationBindings(BTreeMap<PublicKey, PublicKey>);

impl DestinationBindings {
	pub(crate) fn from_arkoor_parts(
		parts: &[protos::ArkoorCosignRequest], required: bool, network: Network,
		server_pubkey: PublicKey, server_mailbox_key: &Keypair,
	) -> anyhow::Result<Self> {
		if !required { return Ok(Self::default()); }
		let mut ret = Self::default();
		for output in parts.iter().flat_map(|part| part.outputs.iter().chain(&part.isolated_outputs)) {
			let policy = VtxoPolicy::deserialize(&output.policy).context("invalid arkoor output policy")?;
			let key = policy.user_pubkey();
			let mailbox = match &output.address {
				Some(text) => {
					let address: ark::Address = text.parse().context("invalid arkoor recipient address")?;
					if address.is_testnet() != (network != Network::Bitcoin)
						|| !address.is_for_server(server_pubkey) || address.policy() != &policy {
						bail!("arkoor recipient address differs from output or server");
					}
					let blinded = match address.delivery() {
						[ark::address::VtxoDelivery::ServerMailbox { blinded_id }] => *blinded_id,
						_ => bail!("arkoor recipient requires one upstream mailbox delivery"),
					};
					Some(MailboxIdentifier::from_blinded(blinded, key, server_mailbox_key)
						.context("invalid arkoor recipient mailbox")?.as_pubkey())
				},
				None => None,
			};
			if let Some(mailbox) = mailbox {
				ret.insert(key, mailbox)?;
			} else if required {
				bail!("arkoor output {key} has no payout destination");
			}
		}
		Ok(ret)
	}

	pub(crate) fn insert(&mut self, key: PublicKey, mailbox: PublicKey) -> anyhow::Result<()> {
		if let Some(previous) = self.0.get(&key) {
			if *previous != mailbox { bail!("output {key} has conflicting payout mailboxes"); }
			return Ok(());
		}
		self.0.insert(key, mailbox);
		Ok(())
	}

	pub(crate) fn mailbox_for(&self, key: PublicKey) -> Option<PublicKey> {
		self.0.get(&key).copied()
	}

	pub(crate) fn bindings_for_package(
		&self, builder: &ArkoorPackageBuilder<ServerCanCosign>, required: bool,
	) -> anyhow::Result<Vec<(VtxoId, PublicKey)>> {
		if !required { return Ok(Vec::new()); }
		let mut keys = BTreeSet::new();
		let mut bindings = Vec::new();
		for vtxo in builder.build_unsigned_vtxos() {
			let key = vtxo.policy().user_pubkey();
			keys.insert(key);
			let mailbox = *self.0.get(&key)
				.with_context(|| format!("output {key} has no admitted payout destination"))?;
			bindings.push((vtxo.id(), mailbox));
		}
		if self.0.keys().any(|key| !keys.contains(key)) {
			bail!("payout destination names no actual output");
		}
		Ok(bindings)
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::secp256k1::{Secp256k1, SecretKey};
	use ark::address::VtxoDelivery;
	use ark::mailbox::BlindedMailboxIdentifier;

	use super::*;

	#[test]
	fn address_unblinds_to_mailbox_and_conflicting_copy_is_refused() {
		let secp = Secp256k1::new();
		let key = |byte| Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[byte; 32]).unwrap());
		let server = key(3);
		let coin = key(4);
		let mailbox = key(5);
		let other_mailbox = key(6);
		let policy = VtxoPolicy::new_pubkey(coin.public_key());
		let output = |mailbox: &Keypair| {
			let blinded_id = MailboxIdentifier::from_pubkey(mailbox.public_key())
				.to_blinded(server.public_key(), &coin).unwrap();
			let address = ark::Address::new(true, server.public_key(), policy.clone(),
				vec![VtxoDelivery::ServerMailbox { blinded_id }]);
			protos::ArkoorDestination {
				policy: policy.serialize(), address: Some(address.to_string()),
				..Default::default()
			}
		};
		let part = |outputs| protos::ArkoorCosignRequest { outputs, ..Default::default() };
		let found = DestinationBindings::from_arkoor_parts(
			&[part(vec![output(&mailbox)])], true, Network::Signet,
			server.public_key(), &server,
		).unwrap();
		assert_eq!(found.mailbox_for(coin.public_key()), Some(mailbox.public_key()));
		assert!(DestinationBindings::from_arkoor_parts(
			&[part(vec![output(&mailbox), output(&other_mailbox)])], true,
			Network::Signet, server.public_key(), &server,
		).is_err());
	}

	#[test]
	fn bare_mailbox_is_not_an_arkoor_destination() {
		let secp = Secp256k1::new();
		let server = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[3; 32]).unwrap());
		let recipient = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[4; 32]).unwrap());
		let part = protos::ArkoorCosignRequest {
			outputs: vec![protos::ArkoorDestination {
				policy: VtxoPolicy::new_pubkey(recipient.public_key()).serialize(),
				..Default::default()
			}], ..Default::default()
		};
		assert!(DestinationBindings::from_arkoor_parts(&[part], true, Network::Signet,
			server.public_key(), &server).is_err());
	}

	#[test]
	fn altered_valid_address_recovers_payer_mailbox_not_recipient() {
		let secp = Secp256k1::new();
		let key = |byte| Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[byte; 32]).unwrap());
		let server = key(3);
		let recipient_coin = key(4);
		let recipient_mailbox = key(5);
		let payer_mailbox = key(6);
		let policy = VtxoPolicy::new_pubkey(recipient_coin.public_key());
		let original_blinded = MailboxIdentifier::from_pubkey(recipient_mailbox.public_key())
			.to_blinded(server.public_key(), &recipient_coin).unwrap();
		// B' = B + M_payer - M_recipient. The address still has the recipient's P.
		let changed_point = PublicKey::combine_keys(&[
			&original_blinded.as_pubkey(), &payer_mailbox.public_key(),
			&recipient_mailbox.public_key().negate(&secp),
		]).unwrap();
		let altered = ark::Address::new(true, server.public_key(), policy.clone(), vec![
			VtxoDelivery::ServerMailbox { blinded_id: BlindedMailboxIdentifier::from_pubkey(changed_point) },
		]);
		let part = protos::ArkoorCosignRequest {
			outputs: vec![protos::ArkoorDestination {
				policy: policy.serialize(), address: Some(altered.to_string()),
				..Default::default()
			}], ..Default::default()
		};
		let found = DestinationBindings::from_arkoor_parts(&[part], true, Network::Signet,
			server.public_key(), &server).unwrap();
		assert_eq!(found.mailbox_for(recipient_coin.public_key()), Some(payer_mailbox.public_key()));
		assert_ne!(found.mailbox_for(recipient_coin.public_key()), Some(recipient_mailbox.public_key()));
	}
}
