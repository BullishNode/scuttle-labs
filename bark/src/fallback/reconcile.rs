//! Adopt authenticated outcomes on known coins, without a payout or ancestry ledger.

use std::collections::{BTreeSet, HashSet};
use anyhow::Context;
use ark::{ProtocolEncoding, Vtxo, VtxoId, VtxoPolicy};
use ark::lightning::PaymentHash;
use ark::vtxo::{Full, VtxoRef};
use bitcoin::secp256k1::PublicKey;
use lightning_invoice::Bolt11Invoice;
use server_rpc::protos::{self, VtxoSpendState};
use crate::Wallet;
use crate::vtxo::{FallbackStatus, VtxoStateKind};

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
		ensure!(!grants.is_empty() && PaymentHash::from(invoice) == hash, "receive association is incomplete");
		let (mut srv, _) = self.require_server_with_deadline(
			Some(bark_runtime::Instant::now() + crate::SERVER_READ_TIMEOUT)).await?;
		let response = Self::server_read(Some(bark_runtime::Instant::now() + crate::SERVER_READ_TIMEOUT),
			srv.client.check_lightning_receive(protos::CheckLightningReceiveRequest {
			hash: hash.to_vec(), wait: false,
		})).await??.into_inner();
		ensure!(response.invoice == invoice.to_string()
			&& Some(response.amount_sat) == invoice.amount_milli_satoshis().map(|msat| msat / 1000),
			"receive status differs from the original signed invoice");
		let actual = response.htlc_vtxos.iter().map(|raw| VtxoId::from_slice(raw)).collect::<Result<BTreeSet<_>, _>>()?;
		ensure!(actual.len() == response.htlc_vtxos.len() && actual.len() == grants.len()
			&& actual == grants.iter().copied().collect(), "receive status differs from the original grants");
		response.fallback_spend_state.map(|raw| {
			let state = VtxoSpendState::try_from(raw).context("unknown receive fallback status")?;
			FallbackStatus::try_from(state)
		}).transpose()
	}

	pub(crate) async fn apply_receive_fallback(&self, hash: PaymentHash, grants: &[VtxoId], status: FallbackStatus)
		-> anyhow::Result<()>
	{
		// Caller holds FallbackHistory across the status request and adoption.
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
		ensure!(PublicKey::from_slice(owner)? == self.mailbox_keypair().public_key(),
			"server coin ownership does not match this mailbox");
		// Every status-bearing RPC caller holds FallbackHistory before sending
		// its request, so an older reply cannot overwrite a newer reorg outcome.
		let stored = self.inner.db.get_wallet_vtxo(id).await?;
		if committed {
			let status = FallbackStatus::try_from(state)?;
			if stored.is_some() { self.inner.db.set_vtxo_fallback_status(id, status).await?; }
		} else {
			ensure!(!matches!(state, VtxoSpendState::FallbackPending | VtxoSpendState::FallbackForfeited),
				"fallback status is missing its commitment marker");
			ensure!(stored.is_none_or(|coin| coin.fallback_status.is_none()),
				"missing fallback status cannot release a known commitment");
		}
		Ok(state)
	}

	pub(crate) async fn has_fallback_commitment(&self, id: VtxoId) -> anyhow::Result<bool> {
		Ok(self.inner.db.get_wallet_vtxo(id).await?.is_some_and(|coin| coin.fallback_status.is_some()))
	}

	pub(crate) async fn fallback_inputs_terminal(&self, ids: &[VtxoId]) -> anyhow::Result<bool> {
		ensure!(!ids.is_empty(), "fallback operation has no known inputs");
		let mut keyed = Vec::new();
		for id in ids {
			let coin = self.inner.db.get_wallet_vtxo(*id).await?
				.ok_or_else(|| anyhow::anyhow!("retained fallback input is missing"))?;
			keyed.push((*id, self.get_vtxo_key(&coin.vtxo).await?));
		}
		let queries = keyed.iter().map(|(id,key)| (*id,key)).collect::<Vec<_>>();
		let states = self.fetch_vtxo_spend_states(&queries).await?;
		let mut terminal = true;
		for id in ids {
			ensure!(states.contains_key(id), "fallback operation is missing an authenticated input status");
			let coin = self.inner.db.get_wallet_vtxo(*id).await?
				.ok_or_else(|| anyhow::anyhow!("retained fallback input disappeared"))?;
			let status = coin.fallback_status.ok_or_else(|| anyhow::anyhow!("operation input has no committed fallback status"))?;
			terminal &= status != FallbackStatus::Pending;
		}
		Ok(terminal)
	}

	pub(crate) async fn trust_ordinary_spends(&self, ids: &[VtxoId]) -> anyhow::Result<()> {
		for id in ids {
			ensure!(self.inner.db.get_wallet_vtxo(*id).await?.is_none_or(|coin|
				coin.fallback_status != Some(FallbackStatus::Pending)),
				"ordinary spend cannot erase a pending fallback commitment");
		}
		self.mark_vtxos_as_spent(ids).await
	}

	pub(crate) async fn retain_fallback_pending(&self, coins: &[Vtxo<Full>]) -> anyhow::Result<()> {
		for coin in coins {
			self.validate_vtxo(coin).await?;
			let key = self.get_vtxo_key(coin).await?;
			let _guard = self.lock_fallback_history(std::time::Duration::from_secs(30)).await?;
			let (state, committed) = self.fetch_vtxo_status_locked(coin.id(), &key).await?;
			ensure!(committed, "retained source has no authenticated commitment");
			self.inner.db.store_fallback_vtxo(coin, FallbackStatus::try_from(state)?).await?;
			ensure!(self.get_full_vtxo(coin.id()).await?.serialize() == coin.serialize(), "retained fallback body changed");
		}
		Ok(())
	}

	pub(crate) async fn filter_expired_refresh_inputs<V: VtxoRef>(&self, inputs: Vec<V>) -> anyhow::Result<Vec<V>> {
		let tip = self.inner.chain.tip().await?;
		let mut keyed = Vec::new();
		let mut excluded = HashSet::new();
		for input in &inputs {
			if let Some(stored) = self.inner.db.get_wallet_vtxo(input.vtxo_id()).await? {
				if stored.state.kind() == VtxoStateKind::Spent || stored.fallback_status.is_some() {
					excluded.insert(stored.id());
					continue;
				}
				if stored.expiry_height() <= tip { keyed.push((stored.id(), self.get_vtxo_key(&stored.vtxo).await?)); }
			}
		}
		let queries = keyed.iter().map(|(id,key)| (*id,key)).collect::<Vec<_>>();
		let states = self.fetch_vtxo_spend_states(&queries).await?;
		let queried = keyed.iter().map(|(id,_)| *id).collect::<HashSet<_>>();
		Ok(inputs.into_iter().filter(|input| !excluded.contains(&input.vtxo_id())
			&& (!queried.contains(&input.vtxo_id()) || states.get(&input.vtxo_id()) == Some(&VtxoSpendState::Spendable))).collect())
	}
}
