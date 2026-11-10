//! Retain the validated destination of a receive before collecting its invoice.

use anyhow::Context;
use bitcoin::secp256k1::PublicKey;

use ark::{ProtocolEncoding, Vtxo, VtxoId, VtxoPolicy};
use ark::arkoor::state::ServerCanCosign;
use ark::arkoor::package::ArkoorPackageBuilder;
use ark::lightning::PaymentHash;
use ark::vtxo::Full;

use super::Tx;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReceiveClaimBinding {
	pub source: VtxoId,
	pub payment_hash: PaymentHash,
	pub output: Vtxo<Full>,
	pub output_mailbox: Option<PublicKey>,
}

impl ReceiveClaimBinding {
	pub(crate) fn from_package(builder: &ArkoorPackageBuilder<ServerCanCosign>,
		destinations: &crate::destination::DestinationBindings,
	) -> anyhow::Result<Vec<Self>> {
		builder.builders.iter().filter_map(|part| {
			let source = part.input();
			let hash = match source.policy() {
				VtxoPolicy::ServerHtlcRecv(p) => p.payment_hash,
				VtxoPolicy::ServerHtlcRecv_v0(p) => p.payment_hash,
				_ => return None,
			};
			Some((|| {
				let mut outputs = part.build_unsigned_vtxos();
				let output = outputs.next().context("receive claim has no final output")?;
				ensure!(matches!(output.policy(), VtxoPolicy::Pubkey(_)), "receive claim destination must be a pubkey policy");
				ensure!(outputs.next().is_none(), "receive claim has multiple final outputs");
				ensure!(output.amount() == source.amount(), "receive claim does not preserve its source amount");
				let output_mailbox = destinations.mailbox_for(output.policy().user_pubkey());
				Ok(Self { source: source.id(), payment_hash: hash, output, output_mailbox })
			})())
		}).collect()
	}
}


impl Tx<'_> {
	pub(crate) async fn prepare_receive_claims(&self, bindings: &[ReceiveClaimBinding]) -> anyhow::Result<()> {
		let mut ids = bindings.iter().map(|binding| binding.source).collect::<Vec<_>>();
		ids.sort_unstable();
		super::expiry_guard::guard_obligation_changes(self, &ids).await?;
		for binding in bindings {
			let subscription: i64 = self.query_opt("SELECT v.lightning_htlc_subscription_id FROM vtxo v \
				JOIN htlc_vtxo h ON h.id=v.id JOIN lightning_htlc_subscription s ON s.id=v.lightning_htlc_subscription_id \
				WHERE v.vtxo_id=$1 AND h.direction='outgoing' AND h.payment_hash=$2 AND s.payment_hash=$2 \
				AND s.status IN ('htlcs-ready','settled')", &[&binding.source.to_string(), &binding.payment_hash.to_string()]).await?
				.context("receive source is not its subscription's grant")?.get(0);
			if self.expiry_payouts() {
				ensure!(binding.output_mailbox.is_some(), "receive claim has no output payout mailbox");
				if let Some(birth) = self.coin_owner_mailboxes(&[binding.output.id()]).await?
					.get(&binding.output.id()) {
					ensure!(Some(*birth) == binding.output_mailbox,
						"existing receive output differs from its claim mailbox");
				}
			}
			self.execute("INSERT INTO expiry_lightning_receive_claim \
				(source_vtxo_id,subscription_id,payment_hash,output_vtxo_id,output_vtxo,output_mailbox) VALUES($1,$2,$3,$4,$5,$6) \
				ON CONFLICT(source_vtxo_id) DO NOTHING", &[&binding.source.to_string(), &subscription,
				&binding.payment_hash.to_string(), &binding.output.id().to_string(), &binding.output.serialize(),
				&binding.output_mailbox.map(|key| key.to_string())]).await?;
			let row = self.query_one("SELECT subscription_id,payment_hash,output_vtxo,output_mailbox FROM expiry_lightning_receive_claim \
				WHERE source_vtxo_id=$1", &[&binding.source.to_string()]).await?;
			let output = Vtxo::<Full>::deserialize(row.get("output_vtxo"))?;
			ensure!(row.get::<_, i64>("subscription_id") == subscription
				&& row.get::<_, &str>("payment_hash") == binding.payment_hash.to_string()
				&& output.serialize() == binding.output.serialize()
				&& row.get::<_, Option<&str>>("output_mailbox")
					== binding.output_mailbox.map(|key| key.to_string()).as_deref(),
				"receive claim conflicts with its original destination");
			self.execute("UPDATE vtxo SET updated_at=NOW() WHERE vtxo_id=$1", &[&binding.source.to_string()]).await?;
		}
		Ok(())
	}

	/// Only upstream settlement or fulfillment makes the recorded destination payable.
	pub(crate) async fn receive_claim_recipient(&self, id: VtxoId) -> anyhow::Result<Option<(VtxoId, PublicKey, PublicKey, i64)>> {
		let Some(row) = self.query_opt("SELECT c.output_vtxo_id,c.output_vtxo,c.output_mailbox,c.subscription_id FROM expiry_lightning_receive_claim c \
			JOIN vtxo v ON v.vtxo_id=c.source_vtxo_id AND v.lightning_htlc_subscription_id=c.subscription_id \
			JOIN htlc_vtxo h ON h.id=v.id AND h.direction='outgoing' AND h.payment_hash=c.payment_hash \
			JOIN lightning_htlc_subscription s ON s.id=c.subscription_id AND s.payment_hash=c.payment_hash \
			WHERE (c.source_vtxo_id=$1 OR c.output_vtxo_id=$1) \
			AND (s.status='settled' OR h.offchain_resolution='fulfilled' OR h.chain_resolution='fulfilled')",
			&[&id.to_string()]).await? else { return Ok(None); };
		let output = Vtxo::<Full>::deserialize(row.get("output_vtxo"))?;
		let output_id: VtxoId = row.get::<_, &str>("output_vtxo_id").parse()?;
		ensure!(output.id() == output_id, "receive destination identity differs");
		let VtxoPolicy::Pubkey(policy) = output.policy() else { bail!("receive destination is not a pubkey"); };
		let mailbox: PublicKey = row.get::<_, &str>("output_mailbox").parse()?;
		if let Some(birth) = self.coin_owner_mailboxes(&[output_id]).await?.get(&output_id) {
			ensure!(*birth == mailbox, "registered receive output differs from its claim mailbox");
		}
		Ok(Some((output_id, policy.user_pubkey, mailbox, row.get("subscription_id"))))
	}
}
