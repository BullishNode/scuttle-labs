//! Adopt authenticated outcomes on known coins, without a payout or ancestry ledger.

use std::collections::BTreeSet;
use anyhow::Context;
use ark::{ProtocolEncoding, VtxoId, VtxoPolicy};
use ark::attestations::VtxoStatusAttestation;
use ark::lightning::PaymentHash;
use bitcoin::secp256k1::PublicKey;
use lightning_invoice::Bolt11Invoice;
use server_rpc::protos::{self, VtxoSpendState};
use server_rpc::StatusExt;
use crate::Wallet;
use crate::vtxo::FallbackStatus;

impl TryFrom<VtxoSpendState> for FallbackStatus {
	type Error = anyhow::Error;
	fn try_from(state: VtxoSpendState) -> anyhow::Result<Self> {
		Ok(match state {
			VtxoSpendState::FallbackPending => Self::Pending,
			VtxoSpendState::Spent => Self::Spent,
			VtxoSpendState::FallbackForfeited => Self::Forfeited,
			_ => bail!("fallback commitment has a contradictory spend state"),
		})
	}
}

impl Wallet {
	pub(crate) async fn receive_fallback_status(&self, hash: PaymentHash, invoice: &Bolt11Invoice, grants: &[VtxoId])
		-> anyhow::Result<Option<FallbackStatus>>
	{
		ensure!(!grants.is_empty() && PaymentHash::from(invoice) == hash, "receive invoice or grant set is incomplete");
		let (mut srv, _) = self.require_server().await?;
		let response = srv.client.check_lightning_receive(protos::CheckLightningReceiveRequest {
			hash: hash.to_vec(), wait: false,
		}).await?.into_inner();
		ensure!(response.invoice == invoice.to_string()
			&& Some(response.amount_sat) == invoice.amount_milli_satoshis().map(|msat| msat / 1000),
			"receive status differs from the original signed invoice");
		let actual = response.htlc_vtxos.iter().map(|raw| VtxoId::from_slice(raw)).collect::<Result<BTreeSet<_>, _>>()?;
		ensure!(actual.len() == response.htlc_vtxos.len() && actual.len() == grants.len()
			&& actual == grants.iter().copied().collect(), "receive status differs from the original grants");
		if response.fallback_spend_state.is_some() {
			// Validate every grant before changing any local payout status. The
			// hash response is an aggregate, not an owner attestation.
			let mailbox = self.mailbox_keypair().public_key().serialize().to_vec();
			for id in grants {
				let coin = self.inner.db.get_wallet_vtxo(*id).await?
					.context("receive grant is not retained")?;
				let key = self.get_vtxo_key(&coin.vtxo).await?;
				let attestation = VtxoStatusAttestation::new(*id, &key);
				let status = srv.client.get_vtxo_status(protos::GetVtxoStatusRequest {
					vtxo_id: id.to_bytes().to_vec(), attestation: attestation.serialize(),
				}).await?.into_inner();
				if status.owner_mailbox != mailbox {
					log::warn!("Receive grant {id} payout belongs to another mailbox; keeping the action parked");
					return Ok(None);
				}
			}
		}
		response.fallback_spend_state.map(|raw| {
			let state = VtxoSpendState::try_from(raw).context("unknown receive fallback status")?;
			FallbackStatus::try_from(state)
		}).transpose()
	}

	pub(crate) async fn apply_receive_fallback(&self, hash: PaymentHash, grants: &[VtxoId], status: FallbackStatus)
		-> anyhow::Result<()>
	{
		// The original invoice and complete grant set were matched before adoption.
		for id in grants {
			let coin = self.inner.db.get_wallet_vtxo(*id).await?.context("receive grant is not retained")?;
			let payment_hash = match coin.policy() {
				VtxoPolicy::ServerHtlcRecv(policy) => policy.payment_hash,
				VtxoPolicy::ServerHtlcRecv_v0(policy) => policy.payment_hash,
				_ => bail!("receive grant has another policy"),
			};
			ensure!(payment_hash == hash, "receive grant belongs to another invoice");
		}
		for id in grants { self.inner.db.set_vtxo_fallback_status(*id, status).await?; }
		Ok(())
	}

	pub(crate) async fn observe_server_vtxo_status(&self, id: VtxoId, owner: &[u8],
		state: VtxoSpendState, committed: bool) -> anyhow::Result<VtxoSpendState>
	{
		if !self.properties().await?.server_payouts.context("server payout capability is not known")? {
			return Ok(state);
		}
		ensure!(PublicKey::from_slice(owner)? == self.mailbox_keypair().public_key(),
			"server coin ownership does not match this mailbox");
		let stored = self.inner.db.get_wallet_vtxo(id).await?;
		if committed {
			let status = FallbackStatus::try_from(state)?;
			if stored.is_some() { self.inner.db.set_vtxo_fallback_status(id, status).await?; }
		} else {
			ensure!(!matches!(state, VtxoSpendState::FallbackPending | VtxoSpendState::FallbackForfeited),
				"fallback status is missing its commitment marker");
		}
		Ok(state)
	}

	/// Adopt only named, authenticated inputs of this rejected operation.
	pub(crate) async fn adopt_fallback_refusal(&self, error: &anyhow::Error, inputs: &[VtxoId]) -> anyhow::Result<()> {
		let ids = error.chain().find_map(|cause| cause.downcast_ref::<tonic::Status>())
			.map(|status| status.rejected_vtxos()).unwrap_or_default();
		ensure!(!ids.is_empty() && ids.iter().all(|id| inputs.contains(id)),
			"fallback refusal does not name this operation's inputs");
		ensure!(self.adopt_committed_inputs(&ids).await?, "fallback refusal has no authenticated commitment");
		Ok(())
	}

	pub(crate) async fn adopt_committed_inputs(&self, ids: &[VtxoId]) -> anyhow::Result<bool> {
		ensure!(!ids.is_empty(), "fallback operation has no known inputs");
		for id in ids {
			let coin = self.inner.db.get_wallet_vtxo(*id).await?.context("fallback input is missing")?;
			let key = self.get_vtxo_key(&coin.vtxo).await?;
			let (_, committed) = self.fetch_vtxo_status(*id, &key).await?;
			if !committed { return Ok(false); }
		}
		Ok(true)
	}

}
