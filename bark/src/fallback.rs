//! Expiry payouts: the payout destination of each coin, and what the server
//! decided for a coin once it expired.

use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use bitcoin::{Amount, ScriptBuf};
use bitcoin::secp256k1::{Keypair, PublicKey};
use log::warn;

use ark::{ProtocolEncoding, VtxoId, VtxoPolicy};
use ark::attestations::VtxoStatusAttestation;
use ark::vtxo::VtxoRef;
use server_rpc::MAX_NB_VTXO_IDS;
use server_rpc::protos::{self, VtxoSpendState};

use crate::Wallet;
use crate::vtxo::{FallbackPayout, FallbackStatus, VtxoLockHolder, VtxoState, VtxoStateKind};

/// One coin's status as a server that pays out expired coins reports it.
pub(crate) struct ServerVtxoStatus {
	pub state: VtxoSpendState,
	/// Whether the coin's expiry payout is bound to this wallet's mailbox key.
	pub own_mailbox: bool,
	/// The payout decided for the coin, if any.
	pub fallback: Option<FallbackStatus>,
}

impl ServerVtxoStatus {
	/// Whether the coin can no longer be spent in Ark.
	pub fn is_spent(&self) -> bool {
		self.fallback.is_some() || self.state == VtxoSpendState::Spent
	}
}

impl Wallet {
	/// Whether the connected server pays out expired coins.
	///
	/// `false` without a connection: every caller acts on a server reply.
	pub(crate) async fn server_payouts(&self) -> bool {
		match self.inner.server.get() {
			Some(srv) => srv.scuttle_payouts().await,
			None => false,
		}
	}

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
		let (srv, ark_info) = self.require_server().await?;
		if !srv.scuttle_payouts().await {
			return Ok(());
		}
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
					_ => bail!("recipient address requires one server mailbox delivery"),
				}
			} else {
				let (_, keypair) = self.pubkey_keypair(&key).await?
					.context("arkoor output to another wallet has no recipient address")?;
				self.mailbox_identifier().to_blinded(ark_info.mailbox_pubkey, &keypair)?
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

	/// Ask the server what it decided for this coin's expiry payout: its
	/// status, the coin value, the fallback fee and the payout to the wallet's
	/// fallback address. Returns `None` while no payout has been decided.
	///
	/// This is the server's report. Confirm the payout on-chain at `address`
	/// before counting it as received.
	pub async fn fallback_payout(&self, vtxo_id: VtxoId) -> anyhow::Result<Option<FallbackPayout>> {
		let coin = self.get_vtxo_by_id(vtxo_id).await?;
		let key = self.get_vtxo_key(&coin.vtxo).await?;
		let (mut srv, _) = self.require_server().await?;
		let response = srv.client.get_vtxo_status(protos::GetVtxoStatusRequest {
			vtxo_id: vtxo_id.to_bytes().to_vec(),
			attestation: VtxoStatusAttestation::new(vtxo_id, &key).serialize(),
		}).await?.into_inner();
		let Some(amounts) = response.fallback_payout else { return Ok(None); };
		let mailbox = PublicKey::from_slice(&response.owner_mailbox)?;
		ensure!(mailbox == self.mailbox_keypair().public_key(), "fallback payout belongs to another mailbox");
		let status = fallback_status(response.spend_state, true)?
			.context("fallback payout without a fallback status")?;
		let network = self.properties().await?.network;
		Ok(Some(FallbackPayout {
			status,
			gross: Amount::from_sat(amounts.gross_sat),
			fee: Amount::from_sat(amounts.fee_sat),
			payout: Amount::from_sat(amounts.payout_sat),
			address: bitcoin::Address::from_script(&ark::fallback::payout_script(mailbox), network)?,
		}))
	}

	/// Ask a server that pays out expired coins for the status of each coin.
	///
	/// A coin the server does not know, whose batch request failed, or whose
	/// answer is unusable, is absent from the result. Only a server that
	/// cannot be reached is an error.
	pub(crate) async fn fetch_vtxo_statuses(
		&self,
		queries: &[(VtxoId, &Keypair)],
	) -> anyhow::Result<HashMap<VtxoId, ServerVtxoStatus>> {
		let mut ret = HashMap::with_capacity(queries.len());
		if queries.is_empty() {
			return Ok(ret);
		}

		let mailbox = self.mailbox_keypair().public_key().serialize();
		let (mut srv, _) = self.require_server().await?;
		for chunk in queries.chunks(MAX_NB_VTXO_IDS) {
			let req = protos::GetVtxosStatusByIdsRequest {
				vtxos: chunk.iter().map(|(id, keypair)| protos::VtxoStatusQuery {
					vtxo_id: id.to_bytes().to_vec(),
					attestation: VtxoStatusAttestation::new(*id, keypair).serialize(),
				}).collect(),
			};
			let resp = match srv.client.get_vtxos_status_by_ids(req).await {
				Ok(resp) => resp.into_inner(),
				Err(e) => {
					warn!("Could not fetch the status of {} vtxos from server: {:#}", chunk.len(), e);
					continue;
				},
			};

			for status in resp.vtxos {
				let Some(id) = VtxoId::from_slice(&status.vtxo_id).ok()
					.filter(|id| chunk.iter().any(|(asked, _)| asked == id))
				else {
					warn!("Server returned a status for a vtxo we did not ask for: {:?}", status.vtxo_id);
					continue;
				};
				let (Ok(state), Ok(fallback)) = (
					VtxoSpendState::try_from(status.spend_state),
					fallback_status(status.spend_state, status.fallback_committed),
				) else {
					warn!("Server returned an unusable status for vtxo {id}; this wallet may need updating");
					continue;
				};
				ret.insert(id, ServerVtxoStatus {
					state,
					own_mailbox: status.owner_mailbox == mailbox,
					fallback,
				});
			}
		}
		Ok(ret)
	}

	/// The server's status of a coin that must pay out to this wallet's mailbox.
	pub(crate) async fn fetch_own_vtxo_status(
		&self,
		vtxo_id: VtxoId,
		keypair: &Keypair,
	) -> anyhow::Result<ServerVtxoStatus> {
		let status = self.fetch_vtxo_statuses(&[(vtxo_id, keypair)]).await?.remove(&vtxo_id)
			.with_context(|| format!("server does not know vtxo {vtxo_id}"))?;
		ensure!(status.own_mailbox, "the payout of vtxo {vtxo_id} is bound to another mailbox");
		Ok(status)
	}

	/// Ask the server whether it decided an expiry payout for any of these
	/// stored coins, and retire each coin it did: spent with its payout status
	/// when the payout goes to this wallet's mailbox, plain spent when it goes
	/// to another one.
	///
	/// Returns whether any coin was decided. Always `false` on a server that
	/// does not pay out expired coins.
	pub(crate) async fn check_fallback(&self, ids: &[VtxoId]) -> anyhow::Result<bool> {
		if ids.is_empty() || !self.server_payouts().await {
			return Ok(false);
		}

		let mut keys = Vec::with_capacity(ids.len());
		for id in ids {
			let vtxo = self.get_vtxo_by_id(*id).await?;
			keys.push((*id, self.get_vtxo_key(&vtxo.vtxo).await?));
		}
		let queries = keys.iter().map(|(id, key)| (*id, key)).collect::<Vec<_>>();

		let mut decided = false;
		for (id, status) in self.fetch_vtxo_statuses(&queries).await? {
			let Some(fallback) = status.fallback else { continue };
			if status.own_mailbox {
				self.inner.db.set_vtxo_fallback_status(id, fallback).await?;
			} else {
				self.mark_vtxos_as_spent([id]).await?;
			}
			decided = true;
		}
		Ok(decided)
	}

	/// [Wallet::check_fallback], and whether it left every one of these coins
	/// spent: the action holding them has nothing left to wait for.
	pub(crate) async fn check_fallback_all_spent(&self, ids: &[VtxoId]) -> anyhow::Result<bool> {
		if !self.check_fallback(ids).await? {
			return Ok(false);
		}
		for id in ids {
			if self.get_vtxo_by_id(*id).await?.state.kind() != VtxoStateKind::Spent {
				return Ok(false);
			}
		}
		Ok(true)
	}

	/// Check the coins that expired and are still held, and the ones whose
	/// payout was pending at the last check.
	pub async fn sync_fallback_payouts(&self) -> anyhow::Result<()> {
		if !self.server_payouts().await {
			return Ok(());
		}

		let tip = self.chain().tip().await?;
		let mut ids = Vec::new();
		for vtxo in self.inner.db.get_all_vtxos().await? {
			if vtxo.state == (VtxoState::Locked { holder: Some(VtxoLockHolder::RoundPayoutCheck) }) {
				if let Err(e) = self.release_verified_round_output(&vtxo.vtxo).await {
					warn!("Round output {} stays locked until the server confirms its \
						payout mailbox: {:#}", vtxo.id(), e);
				}
			}
			let held = vtxo.expiry_height() <= tip
				&& VtxoStateKind::UNSPENT_STATES.contains(&vtxo.state.kind());
			if held || vtxo.fallback_status == Some(FallbackStatus::Pending) {
				ids.push(vtxo.id());
			}
		}
		self.check_fallback(&ids).await?;
		Ok(())
	}

	/// A completed round may be stored while status RPC is unavailable. Keep
	/// its output locked until the server attests this wallet's mailbox.
	pub(crate) async fn release_verified_round_output(&self, output: impl VtxoRef + Copy) -> anyhow::Result<()> {
		let key = self.get_vtxo_key(output).await?;
		let status = self.fetch_own_vtxo_status(output.vtxo_id(), &key).await?;
		ensure!(status.state == VtxoSpendState::Spendable && status.fallback.is_none(),
			"round output is not spendable on the server");
		self.unlock_vtxos([output.vtxo_id()], Some(VtxoLockHolder::RoundPayoutCheck)).await
	}
}

/// The payout status in a status reply, if the server decided one.
fn fallback_status(spend_state: i32, committed: bool) -> anyhow::Result<Option<FallbackStatus>> {
	if !committed {
		return Ok(None);
	}
	Ok(Some(match VtxoSpendState::try_from(spend_state) {
		Ok(VtxoSpendState::FallbackPending) => FallbackStatus::Pending,
		Ok(VtxoSpendState::Spent) => FallbackStatus::Spent,
		Ok(VtxoSpendState::FallbackForfeited) => FallbackStatus::Forfeited,
		_ => bail!("fallback payout with a contradictory spend state"),
	}))
}
