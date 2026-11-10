//! Exact signed payout parents and their allocated backing.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use bitcoin::{Amount, ScriptBuf, Transaction, TxOut};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::secp256k1::PublicKey;
use bitcoin_ext::{BlockHeight, BlockRef, TxOutExt};
use ark::{ProtocolEncoding, ServerVtxo, ServerVtxoPolicy, VtxoId, VtxoPolicy};

use super::expiry_lightning::LightningSourceState;
use super::expiry_ownership::OwnershipBindingSource;
use super::htlc_vtxo::{self, HtlcDirection, HtlcResolution};
use super::ln::{LightningHtlcSubscription, LightningHtlcSubscriptionStatus};
use super::Tx;

pub(crate) const MAX_PAYOUT_VSIZE: usize = 100_000;

pub(crate) struct FallbackClaim {
	pub vtxo_id: VtxoId,
	pub source_vtxo_ids: BTreeSet<VtxoId>,
	pub binding_source: Option<OwnershipBindingSource>,
	pub user_pubkey: PublicKey,
	pub owner_mailbox: PublicKey,
	pub gross: Amount,
	pub payout: Amount,
	pub confirmed_block: Option<BlockRef>,
	/// Canonical N6 user discharge, independent of the superseded payout parent.
	pub user_claimed: bool,
}

fn binding_source_from_evidence(evidence: &serde_json::Value)
	-> anyhow::Result<Option<OwnershipBindingSource>>
{
	let Some(source) = evidence.get("binding_source") else { return Ok(None); };
	let kind = source.get("kind").and_then(serde_json::Value::as_str)
		.context("payout evidence has no binding source kind")?;
	let parsed = match kind {
		"coin" => OwnershipBindingSource::Coin(source.get("id").and_then(serde_json::Value::as_str)
			.context("payout evidence has no coin id")?.parse()?),
		"input_coins" => OwnershipBindingSource::InputCoins(source.get("ids").and_then(serde_json::Value::as_array)
			.context("payout evidence has no input coin ids")?.iter()
			.map(|id| -> anyhow::Result<VtxoId> { Ok(id.as_str().context("invalid input coin id")?.parse()?) })
			.collect::<anyhow::Result<Vec<_>>>()?),
		"subscription" => OwnershipBindingSource::Subscription(source.get("id").and_then(serde_json::Value::as_i64)
			.context("payout evidence has no subscription id")?),
		"allocation" => OwnershipBindingSource::Allocation(source.get("id").and_then(serde_json::Value::as_i64)
			.context("payout evidence has no allocation id")?),
		_ => bail!("unknown payout binding source kind"),
	};
	Ok(Some(parsed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackOutcome {
	Pending,
	Spent,
	Forfeited,
}

/// A known source cannot retire because only its first paid portion was
/// returned. All of its principal must be covered by canonical decisions.
fn fallback_outcome<'a>(principal: Amount, claims: impl IntoIterator<Item = &'a FallbackClaim>)
	-> anyhow::Result<Option<FallbackOutcome>>
{
	let mut covered = Amount::ZERO;
	let mut pending = false;
	let mut paid = false;
	let mut any = false;
	for claim in claims {
		any = true;
		covered = covered.checked_add(claim.gross).context("fallback source coverage overflow")?;
		pending |= claim.confirmed_block.is_none() && !claim.user_claimed;
		paid |= claim.payout > Amount::ZERO || claim.user_claimed;
	}
	ensure!(covered <= principal, "fallback decisions exceed their known source");
	Ok(if !any { None }
		else if pending || covered != principal { Some(FallbackOutcome::Pending) }
		else if paid { Some(FallbackOutcome::Spent) }
		else { Some(FallbackOutcome::Forfeited) })
}

fn receive_timeout_allowed(
	status: LightningHtlcSubscriptionStatus,
	offchain: Option<HtlcResolution>,
	onchain: Option<HtlcResolution>,
	preimage_known: bool,
) -> bool {
	if offchain == Some(HtlcResolution::Fulfilled) || onchain == Some(HtlcResolution::Fulfilled) {
		return false;
	}
	match status {
		LightningHtlcSubscriptionStatus::Canceled => true,
		LightningHtlcSubscriptionStatus::HtlcsReady => !preimage_known,
		_ => false,
	}
}

pub struct PendingExpiryPayout {
	pub id: i64,
	pub inputs: Vec<ServerVtxo>,
	pub payouts: Vec<TxOut>,
	pub parent: Option<Transaction>,
	pub parent_fee: Option<Amount>,
}

impl Tx<'_> {
	/// Follow only immutable registered OOR edges. The stage resolver below
	/// still decides whether a descendant is actually this input owner's right;
	/// sharing a transaction or a mailbox is not sufficient.
	/// Reporting uses a read-only snapshot of the last completed block;
	/// financial decisions validate the chain independently.
	async fn source_fallback_claims(&self, ids: &[VtxoId]) -> anyhow::Result<BTreeMap<VtxoId, FallbackClaim>> {
		if ids.is_empty() { return Ok(BTreeMap::new()); }
		let raw = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
		let rows = self.query("WITH RECURSIVE branch AS (
			SELECT vtxo_id,oor_spent_txid,spent_in_round,offboarded_in FROM vtxo WHERE vtxo_id=ANY($1)
			UNION
			SELECT v.vtxo_id,v.oor_spent_txid,v.spent_in_round,v.offboarded_in
			FROM vtxo v JOIN branch p ON v.vtxo_txid=p.oor_spent_txid
			WHERE p.spent_in_round IS NULL AND p.offboarded_in IS NULL
		) SELECT vtxo_id FROM branch", &[&raw]).await?;
		let mut candidates = ids.iter().copied().collect::<BTreeSet<_>>();
		for row in rows { candidates.insert(row.get::<_, &str>("vtxo_id").parse()?); }
		let candidates = candidates.into_iter().collect::<Vec<_>>();
		let raw = candidates.iter().map(ToString::to_string).collect::<Vec<_>>();
		// Ordinary uncommitted status reads do not depend on watchmand/Core.
		if self.query_opt("SELECT e.id FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
			WHERE e.vtxo_id=ANY($1) AND c.disposition='customer' LIMIT 1", &[&raw]).await?.is_none() {
			return Ok(BTreeMap::new());
		}
		let point = self.expiry_view().await?.context("fallback projection has no published watchman tip")?;
		self.fallback_claims(&candidates, point).await
	}

	/// Aggregate direct and stage-selected input-owner portions onto the
	/// authenticated coin identity. Keep birth ownership separate from payee.
	pub(crate) async fn fallback_outcomes(&self, ids: &[VtxoId])
		-> anyhow::Result<BTreeMap<VtxoId, FallbackOutcome>>
	{
		if !self.expiry_payouts() { return Ok(BTreeMap::new()); }
		let claims = self.source_fallback_claims(ids).await?;
		let sources = self.get_existing_server_vtxos_by_id(ids).await?;
		let owners = self.coin_owner_mailboxes(ids).await?;
		let mut outcomes = BTreeMap::new();
		for (id, source) in sources {
			let Some(owner) = owners.get(&id) else { continue; };
			let mut group = BTreeSet::from([id]);
			loop {
				let before = group.len();
				for claim in claims.values().filter(|claim| claim.owner_mailbox == *owner) {
					if let Some(OwnershipBindingSource::InputCoins(input_ids)) = &claim.binding_source {
						if input_ids.iter().any(|input| group.contains(input)) {
							group.extend(input_ids.iter().copied());
						}
					}
				}
				if group.len() == before { break; }
			}
			let principal = if group.len() == 1 { source.vtxo.amount() } else {
				let all = group.iter().copied().collect::<Vec<_>>();
				let all_sources = self.get_existing_server_vtxos_by_id(&all).await?;
				let all_owners = self.coin_owner_mailboxes(&all).await?;
				ensure!(all_sources.len() == group.len() && all_owners.len() == group.len()
					&& all_owners.values().all(|mailbox| mailbox == owner),
					"multi-input receipt has incomplete owner sources");
				all_sources.values().try_fold(Amount::ZERO, |sum, source|
					sum.checked_add(source.vtxo.amount()).context("multi-input receipt principal overflow"))?
			};
			let matching = claims.values().filter(|claim| claim.owner_mailbox == *owner
				&& claim.source_vtxo_ids.iter().any(|source| group.contains(source)));
			if let Some(outcome) = fallback_outcome(principal, matching)? {
				outcomes.insert(id, outcome);
			}
		}
		Ok(outcomes)
	}

	/// A collected receive can have no final coin row. Use its actual grants
	/// and admitted destination bindings, not the provider's grant ownership.
	/// The public hash-only response receives an outcome, never these keys.
	pub(crate) async fn receive_fallback_outcome(&self, sub: &LightningHtlcSubscription)
		-> anyhow::Result<Option<FallbackOutcome>>
	{
		if !self.expiry_payouts() { return Ok(None); }
		let rows = self.query("SELECT v.vtxo_id,v.amount,c.output_vtxo_id,c.output_vtxo
			FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id AND h.direction='outgoing'
			JOIN lightning_htlc_subscription s ON s.id=v.lightning_htlc_subscription_id AND s.payment_hash=h.payment_hash
			LEFT JOIN expiry_lightning_receive_claim c ON c.source_vtxo_id=v.vtxo_id
				AND c.subscription_id=s.id AND c.payment_hash=s.payment_hash
			WHERE s.id=$1 AND s.payment_hash=$2 ORDER BY v.vtxo_id",
			&[&sub.id, &sub.payment_hash.to_string()]).await?;
		let grants = rows.iter().map(|row| row.get::<_, &str>("vtxo_id").parse())
			.collect::<Result<BTreeSet<VtxoId>, _>>()?;
		ensure!(grants == sub.htlc_vtxos.iter().copied().collect(), "receive grant identities differ from subscription");
		let mut principal = Amount::ZERO;
		let claimant = sub.claim_destination_user_pubkey.or(sub.claim_pubkey)
			.context("receive subscription has no admitted claimant")?;
		let owner = self.receive_subscription_mailbox(sub.id, claimant).await?
			.context("receive subscription has no admitted claimant mailbox")?;
		let mut destinations = BTreeSet::new();
		let mut ids = grants.iter().copied().collect::<Vec<_>>();
		for row in rows {
			let source: VtxoId = row.get::<_, &str>("vtxo_id").parse()?;
			let amount = Amount::from_sat(u64::try_from(row.get::<_, i64>("amount"))?);
			principal = principal.checked_add(amount).context("receive grant amount overflow")?;
			let Some(raw) = row.get::<_, Option<&[u8]>>("output_vtxo") else { continue; };
			let output = ark::Vtxo::<ark::vtxo::Full>::deserialize(raw)?;
			let output_id: VtxoId = row.get::<_, &str>("output_vtxo_id").parse()?;
			ensure!(output.id() == output_id
				&& output.amount() == amount, "receive destination differs from its source");
			let Some((id, key, subscription)) = self.receive_claim_recipient(source).await? else { continue; };
			ensure!(id == output.id(), "collected receive destination differs");
			ensure!(subscription == sub.id && key == claimant,
				"collected receive destination differs from subscription");
			destinations.insert(id);
			ids.push(id);
		}
		let claims = self.source_fallback_claims(&ids).await?;
		let matching = claims.values().filter(|claim| claim.owner_mailbox == owner
			&& ((grants.contains(&claim.vtxo_id)
				&& claim.binding_source == Some(OwnershipBindingSource::Subscription(sub.id)))
				|| destinations.iter().any(|id| claim.source_vtxo_ids.contains(id))));
		fallback_outcome(principal, matching)
	}

	/// Select the winning economic decision before filtering for customer claims.
	/// The receipt describes that decision; it does not promote a pending parent
	/// or replace the wallet's independent canonical-chain confirmation check.
	async fn fallback_claims(&self, ids: &[VtxoId], point: BlockRef) -> anyhow::Result<BTreeMap<VtxoId, FallbackClaim>> {
		let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
		let rows = self.query("SELECT c.vtxo_id,c.owner_mailbox,c.user_pubkey,c.evidence::TEXT AS evidence,c.gross_sat,c.payout_sat,
			CASE WHEN a.status='confirmed'
				AND cv.processed_height >= a.confirmed_height+5
				AND EXISTS(SELECT 1 FROM watchmand_block b WHERE b.height=a.confirmed_height AND b.hash=a.confirmed_hash)
				THEN a.confirmed_height END AS confirmed_height,
			CASE WHEN a.status='confirmed'
				AND cv.processed_height >= a.confirmed_height+5
				AND EXISTS(SELECT 1 FROM watchmand_block b WHERE b.height=a.confirmed_height AND b.hash=a.confirmed_hash)
				THEN a.confirmed_hash END AS confirmed_hash,
			EXISTS(SELECT 1 FROM expiry_terminal_claim t WHERE t.entitlement_id=c.entitlement_id
				 AND cv.processed_height >= t.height+5
				AND EXISTS(SELECT 1 FROM watchmand_block b WHERE b.height=t.height AND b.hash=t.hash)
				AND cv.processed_height=(SELECT MAX(height) FROM watchmand_block)
				AND EXISTS(SELECT 1 FROM watchmand_block b WHERE b.height=cv.processed_height AND b.hash=cv.processed_hash)) AS user_claimed
			FROM (SELECT DISTINCT ON(e.vtxo_id) e.vtxo_id,e.id AS entitlement_id,c.allocation_id,c.owner_mailbox,c.user_pubkey,c.evidence,c.gross_sat,c.fee_sat,
				c.payout_sat,c.forfeited_sat,c.spk,c.disposition FROM expiry_entitlement e
				JOIN expiry_allocation_coin c ON c.entitlement_id=e.id
				JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE e.vtxo_id=ANY($1)
				ORDER BY e.vtxo_id,(a.status='confirmed') DESC,c.allocation_id DESC) c
			JOIN expiry_allocation_state a ON a.id=c.allocation_id
			CROSS JOIN (SELECT $2::BIGINT AS processed_height,$3::TEXT AS processed_hash) cv
			WHERE c.disposition='customer'", &[&ids, &i64::from(point.height), &point.hash.to_string()]).await?;
		let mut claims = BTreeMap::new();
		for row in rows {
			let height: Option<i64> = row.get("confirmed_height");
			let hash: Option<&str> = row.get("confirmed_hash");
			let confirmed_block = match (height, hash) {
				(Some(height), Some(hash)) => Some(BlockRef { height: BlockHeight::new(u32::try_from(height)?), hash: hash.parse()? }),
				(None, None) => None,
				_ => bail!("incomplete fallback confirmation receipt"),
			};
			let evidence: serde_json::Value = serde_json::from_str(row.get("evidence"))?;
			let claim = FallbackClaim {
				vtxo_id: row.get::<_, &str>("vtxo_id").parse()?,
				source_vtxo_ids: BTreeSet::new(),
				binding_source: binding_source_from_evidence(&evidence)?,
				user_pubkey: row.get::<_, &str>("user_pubkey").parse()?,
				owner_mailbox: row.get::<_, &str>("owner_mailbox").parse()?,
				gross: Amount::from_sat(u64::try_from(row.get::<_, i64>("gross_sat"))?),
				payout: Amount::from_sat(u64::try_from(row.get::<_, i64>("payout_sat"))?),
				confirmed_block,
				user_claimed: row.get("user_claimed"),
			};
			let mut claim = claim;
			claim.source_vtxo_ids = self.fallback_claim_sources(&claim).await?;
			claims.insert(claim.vtxo_id, claim);
		}
		Ok(claims)
	}

	/// A receipt belongs to the stage-selected mailbox. Birth ownership is
	/// only one possible source alias: a provisional recipient must not gain
	/// its input owner's refund, and a collected receive grant belongs to its
	/// admitted claimant even if the final output was never inserted.
	async fn fallback_claim_sources(&self, claim: &FallbackClaim) -> anyhow::Result<BTreeSet<VtxoId>> {
		let mut sources = BTreeSet::new();
		if self.coin_owner_mailboxes(&[claim.vtxo_id]).await?.get(&claim.vtxo_id) == Some(&claim.owner_mailbox) {
			sources.insert(claim.vtxo_id);
		}
		match &claim.binding_source {
			Some(OwnershipBindingSource::Coin(id)) => {
				ensure!(*id == claim.vtxo_id && sources.contains(id),
					"coin receipt differs from its admitted birth binding");
			},
			Some(OwnershipBindingSource::InputCoins(ids)) => {
				let actual = self.expiry_input_owner_sources(claim.vtxo_id).await?
					.context("input-owner receipt lost its registered funding lineage")?;
				ensure!(actual.iter().map(|source| source.0).collect::<Vec<_>>() == *ids
					&& actual.iter().all(|source| source.2 == claim.owner_mailbox),
					"input-owner receipt differs from its admitted source bindings");
				sources.extend(ids.iter().copied());
			},
			Some(OwnershipBindingSource::Subscription(id)) => {
				ensure!(self.receive_subscription_mailbox(*id, claim.user_pubkey).await?
					== Some(claim.owner_mailbox), "receive receipt differs from its admitted claimant");
				if self.query_opt("SELECT vtxo_id FROM vtxo WHERE vtxo_id=$1 AND lightning_htlc_subscription_id=$2",
					&[&claim.vtxo_id.to_string(), id]).await?.is_some() {
					sources.insert(claim.vtxo_id);
				}
				if let Some((output, key, subscription)) = self.receive_claim_recipient(claim.vtxo_id).await? {
					ensure!(key == claim.user_pubkey && subscription == *id,
						"receive receipt source differs from its subscription");
					sources.insert(output);
				}
				ensure!(!sources.is_empty(), "receive receipt has no admitted source");
			},
			Some(OwnershipBindingSource::Allocation(id)) => {
				let row = self.query_opt("SELECT c.owner_mailbox FROM expiry_allocation_coin c \
					JOIN expiry_entitlement e ON e.id=c.entitlement_id \
					WHERE c.allocation_id=$1 AND e.vtxo_id=$2 AND c.disposition='customer'",
					&[id, &claim.vtxo_id.to_string()]).await?
					.context("irreversible receipt lost its original allocation")?;
				ensure!(row.get::<_, Option<&str>>(0) == Some(claim.owner_mailbox.to_string().as_str()),
					"irreversible receipt differs from its original allocation");
				sources.insert(claim.vtxo_id);
			},
			None => {},
		}
		Ok(sources)
	}

	/// Select the winning allocation before matching its actual customer. An
	/// unregistered change output can use a fresh key in the same immutable
	/// mailbox as its input owner; a recipient in another mailbox cannot.
	pub(crate) async fn fallback_customer_vtxos(
		&self, vtxos: &[VtxoId],
	) -> anyhow::Result<BTreeSet<VtxoId>> {
		if vtxos.is_empty() { return Ok(BTreeSet::new()); }
		let ids = vtxos.iter().map(ToString::to_string).collect::<Vec<_>>();
		self.query("SELECT q.vtxo_id FROM unnest($1::TEXT[]) q(vtxo_id) \
			JOIN (SELECT DISTINCT ON (e.vtxo_id) e.vtxo_id,c.disposition,c.owner_mailbox \
				FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
				JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE e.vtxo_id=ANY($1) \
				ORDER BY e.vtxo_id,(a.status='confirmed') DESC,c.allocation_id DESC \
			) c ON c.vtxo_id=q.vtxo_id JOIN coin_owner k ON k.vtxo_id=q.vtxo_id \
			WHERE c.disposition='customer' AND c.owner_mailbox=k.mailbox_pk",
			&[&ids]).await?.into_iter().map(|row| Ok(row.get::<_, &str>("vtxo_id").parse()?)).collect()
	}

	/// A group is processed as a whole. Individual-input retries would lose its
	/// cross-tree output aggregation and could charge the output fee twice.
	pub async fn pending_expiry_payouts(&self) -> anyhow::Result<Vec<PendingExpiryPayout>> {
		let rows = self.query("SELECT a.id,a.sweep_tx,p.fee_sat FROM expiry_allocation_state a \
			LEFT JOIN expiry_payout_attempt p ON p.allocation_id=a.id AND p.txid=a.sweep_txid \
			WHERE a.status='pending' ORDER BY a.expiry_height,a.id", &[]).await?;
		let mut ret = Vec::with_capacity(rows.len());
		for row in rows {
			let id: i64 = row.get("id");
			let inputs = self.query("
				SELECT v.vtxo
				FROM expiry_allocation_input i JOIN vtxo v ON v.vtxo_id=i.vtxo_id
				WHERE i.allocation_id=$1
				ORDER BY v.vtxo_id
			", &[&id]).await?
				.into_iter().map(|r| ServerVtxo::deserialize(r.get("vtxo"))).collect::<Result<Vec<_>, _>>()?;
			let payouts = self.query("SELECT spk,SUM(payout_sat)::BIGINT AS amount_sat FROM expiry_allocation_coin WHERE allocation_id=$1 AND disposition='customer' GROUP BY spk HAVING SUM(payout_sat)>0 ORDER BY spk", &[&id]).await?
				.into_iter().map(|r| Ok(TxOut {
					script_pubkey: ScriptBuf::from_hex(r.get("spk"))?,
					value: Amount::from_sat(u64::try_from(r.get::<_, i64>("amount_sat"))?),
				})).collect::<anyhow::Result<Vec<_>>>()?;
			let parent = row.get::<_, Option<&[u8]>>("sweep_tx").map(deserialize).transpose()?;
			let parent_fee = row.get::<_, Option<i64>>("fee_sat")
				.map(|fee| u64::try_from(fee).map(Amount::from_sat)).transpose()?;
			ensure!(parent.is_some() == parent_fee.is_some(),
				"pending payout attempt lacks a recorded fee or transaction");
			ret.push(PendingExpiryPayout { id, inputs, payouts, parent, parent_fee });
		}
		Ok(ret)
	}

	/// Keep every signed replacement and the exact owner outputs before any
	/// network publication. An older attempt can still win on chain.
	pub async fn store_expiry_attempt(
		&self, view: BlockRef, id: i64, payout: &Transaction,
		fee: Amount, operator_spk: &ScriptBuf,
	) -> anyhow::Result<()> {
		self.lock_watchman_tip(view).await?;
		let row = self.query_one("SELECT sweep_txid,sweep_tx FROM expiry_allocation WHERE id=$1 FOR UPDATE", &[&id]).await?;
		ensure!(self.query_one("SELECT status FROM expiry_allocation_state WHERE id=$1", &[&id]).await?.get::<_, &str>(0) == "pending",
			"expiry allocation is no longer pending");
		ensure!(payout.version == bitcoin::transaction::Version::TWO
			&& payout.vsize() <= MAX_PAYOUT_VSIZE
			&& payout.input.iter().any(|input| input.sequence.to_consensus_u32() <= 0xFFFFFFFD)
			&& payout.output.iter().all(TxOutExt::is_standard),
			"direct expiry attempt is nonstandard or lacks RBF");
		let backing = self.query("SELECT v.vtxo FROM expiry_allocation_input i \
			JOIN vtxo v ON v.vtxo_id=i.vtxo_id WHERE i.allocation_id=$1", &[&id]).await?
			.into_iter().map(|row| {
				let root: ServerVtxo = ServerVtxo::deserialize(row.get("vtxo"))?;
				Ok(root.point())
			})
			.collect::<anyhow::Result<BTreeSet<_>>>()?;
		let spent = payout.input.iter().map(|input| input.previous_output)
			.collect::<BTreeSet<_>>();
		ensure!(!backing.is_empty() && backing.is_subset(&spent)
			&& spent.len() == payout.input.len(),
			"direct expiry attempt omits or duplicates allocated backing");
		let expected = self.query("SELECT spk,SUM(payout_sat)::BIGINT AS amount_sat \
			FROM expiry_allocation_coin WHERE allocation_id=$1 AND disposition='customer' \
			GROUP BY spk HAVING SUM(payout_sat)>0", &[&id]).await?
			.into_iter().map(|row| Ok((ScriptBuf::from_hex(row.get("spk"))?,
				Amount::from_sat(u64::try_from(row.get::<_, i64>("amount_sat"))?))))
			.collect::<anyhow::Result<BTreeMap<_, _>>>()?;
		ensure!(!expected.contains_key(operator_spk),
			"operator drain aliases an owner script");
		let owner_outputs = payout.output.iter().filter(|out| expected.contains_key(&out.script_pubkey))
			.collect::<Vec<_>>();
		ensure!(owner_outputs.len() == expected.len() && owner_outputs.iter().all(|out|
			expected.get(&out.script_pubkey) == Some(&out.value)),
			"direct expiry attempt changes owner outputs");
		let other = payout.output.iter().filter(|out| !expected.contains_key(&out.script_pubkey))
			.collect::<Vec<_>>();
		ensure!(other.len() <= 1 && other.iter().all(|out| &out.script_pubkey == operator_spk),
			"direct expiry attempt pays an unknown script");
		let txid = payout.compute_txid().to_string();
		let raw = serialize(payout);
		let fee_sat = i64::try_from(fee.to_sat())?;
		if let Some(old_txid) = row.get::<_, Option<&str>>("sweep_txid") {
			let old = self.query_one("SELECT fee_sat FROM expiry_payout_attempt WHERE allocation_id=$1 AND txid=$2",
				&[&id,&old_txid]).await?;
			if old_txid == txid {
				ensure!(row.get::<_, Option<&[u8]>>("sweep_tx") == Some(raw.as_slice())
					&& old.get::<_, i64>("fee_sat") == fee_sat,
					"stored direct expiry attempt changed bytes or fee");
				return Ok(());
			}
			ensure!(fee_sat > old.get::<_, i64>("fee_sat"),
				"direct expiry replacement does not raise the absolute fee");
		}
		self.execute("INSERT INTO expiry_payout_attempt(allocation_id,txid,signed_tx,fee_sat) \
			VALUES($1,$2,$3,$4)", &[&id,&txid,&raw,&fee_sat]).await?;
		self.execute("UPDATE expiry_allocation SET sweep_txid=$2,sweep_tx=$3 WHERE id=$1",
			&[&id,&txid,&raw]).await?;
		Ok(())
	}

}

impl Tx<'_> {
	/// Shared preimage knowledge cannot authorize an unpaid send's drain claim.
	/// Keep the exact source and its outcome stable through native publication.
	pub async fn lock_watchman_claim_inputs(&self, vtxos: &[ServerVtxo]) -> anyhow::Result<()> {
		if !self.expiry_payouts() { return Ok(()); }
		let ids = vtxos.iter().map(|vtxo| vtxo.id()).collect::<Vec<_>>();
		self.lock_unallocated_watchman_inputs(&ids).await?;
		for candidate in vtxos {
			let (hash, expiry, sending) = match candidate.policy() {
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcSend(policy)) => (policy.payment_hash, policy.htlc_expiry, true),
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcSend_v0(policy)) => (policy.payment_hash, policy.htlc_expiry, true),
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcRecv(policy)) => (policy.payment_hash, policy.htlc_expiry, false),
				ServerVtxoPolicy::User(VtxoPolicy::ServerHtlcRecv_v0(policy)) => (policy.payment_hash, policy.htlc_expiry, false),
				_ => continue,
			};
			let state = self.get_server_vtxo_by_id(candidate.id()).await?;
			ensure!(state.vtxo_id == candidate.id() && state.vtxo.id() == candidate.id()
				&& state.vtxo.to_bare().serialize() == candidate.to_bare().serialize(),
				"Lightning claim source differs from its retained contract");
			ensure!(state.confirmed_height.is_some() && state.oor_spent_txid.is_none()
				&& state.spent_in_round.is_none() && state.offboarded_in.is_none(),
				"Lightning claim source has a successor or lacks confirmation");
			let unspent = self.query_one("SELECT onchain_spent_height IS NULL AND onchain_spent_txid IS NULL AS unspent \
				FROM vtxo WHERE id=$1", &[&state.id]).await?.get::<_, bool>("unspent");
			ensure!(unspent, "Lightning claim source is already spent on chain");
			let htlc = htlc_vtxo::get_htlc_vtxo(self, candidate.id()).await?
				.context("Lightning claim source has no registered HTLC")?;
			let direction = if sending { HtlcDirection::Incoming } else { HtlcDirection::Outgoing };
			ensure!(htlc.vtxo.vtxo_id == candidate.id() && htlc.htlc.direction == direction
				&& htlc.htlc.payment_hash == hash && htlc.htlc.htlc_expiry == expiry,
				"Lightning claim source has a different HTLC");
			if sending {
				ensure!(htlc.htlc.offchain_resolution != Some(HtlcResolution::Revoked)
					&& htlc.htlc.chain_resolution != Some(HtlcResolution::Revoked),
					"Lightning claim source was revoked");
				ensure!(self.lightning_source_state(candidate.id(), hash).await? == LightningSourceState::Discharged,
					"Lightning source is not discharged");
			} else {
				// A preimage can be durable before the native invoice's Settled update.
				self.lock_htlc_settlements().await?;
				let subscription = self.query_opt("SELECT s.status,s.payment_hash FROM lightning_htlc_subscription s \
					JOIN vtxo v ON v.lightning_htlc_subscription_id=s.id WHERE v.id=$1 FOR UPDATE OF s",
					&[&state.id]).await?.context("receive claim source has no subscription")?;
				ensure!(subscription.get::<_, &str>("payment_hash") == hash.to_string(),
					"receive claim source has a different subscription");
				let preimage_known = self.get_htlc_settlement_by_payment_hash(hash).await?.is_some();
				ensure!(receive_timeout_allowed(subscription.get("status"), htlc.htlc.offchain_resolution,
					htlc.htlc.chain_resolution, preimage_known), "receive grant may have been collected");
			}
		}
		Ok(())
	}

	/// A reaction claim must not replace a financial payout already committed
	/// for its physical backing. Use the allocator's sorted coin lock order and
	/// keep these locks through broadcast, including when the root is not itself
	/// one of the economic customer terminals.
	pub async fn lock_unallocated_watchman_inputs(&self, ids: &[ark::VtxoId]) -> anyhow::Result<()> {
		if !self.expiry_payouts() { return Ok(()); }
		let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
		self.query("SELECT vtxo_id FROM vtxo WHERE vtxo_id=ANY($1) ORDER BY vtxo_id FOR UPDATE", &[&ids]).await?;
		// Physical reservations survive while pending/confirmed. An operator
		// entitlement stays reserved until all its allocations have conflicted.
		// Customer entitlements remain irreversible even across a conflict;
		// the off-chain obligation guard retains its separate stronger fence.
		if let Some(row) = self.query_opt("SELECT vtxo_id FROM ( \
			SELECT i.vtxo_id FROM expiry_allocation_input i JOIN expiry_allocation_state a ON a.id=i.allocation_id \
				WHERE i.vtxo_id=ANY($1) AND a.status<>'conflicted' \
			UNION SELECT e.vtxo_id FROM expiry_entitlement e WHERE e.vtxo_id=ANY($1) AND ( \
				NOT EXISTS(SELECT 1 FROM expiry_allocation_coin c WHERE c.entitlement_id=e.id) \
				OR EXISTS(SELECT 1 FROM expiry_allocation_coin c JOIN expiry_allocation_state a ON a.id=c.allocation_id \
					WHERE c.entitlement_id=e.id AND a.status<>'conflicted') \
				OR EXISTS(SELECT 1 FROM expiry_allocation_coin c WHERE c.entitlement_id=e.id AND c.disposition='customer') \
				OR EXISTS(SELECT 1 FROM expiry_terminal_claim p WHERE p.entitlement_id=e.id))) fenced \
			ORDER BY vtxo_id LIMIT 1", &[&ids]).await? {
			return Err(crate::error::FallbackRefusal::DecidedForFallback(row.get::<_, &str>("vtxo_id").parse()?).into());
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use LightningHtlcSubscriptionStatus::{Canceled, Created, Accepted, HtlcsReady, Settled};
	use HtlcResolution::{Fulfilled, Revoked};
	use bitcoin::hashes::Hash;

	#[test]
	fn source_outcome_requires_complete_canonical_coverage() {
		let key = bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[7;32]).unwrap().public_key();
		let mut first = FallbackClaim {
			vtxo_id: ark::test_util::VTXO_VECTORS.board_vtxo.id(), source_vtxo_ids: BTreeSet::new(),
			binding_source: None, user_pubkey: key,
			owner_mailbox: key, gross: Amount::from_sat(40_000),
			payout: Amount::from_sat(39_000),
			confirmed_block: None, user_claimed: false,
		};
		let principal = Amount::from_sat(100_000);
		assert_eq!(fallback_outcome(principal, []).unwrap(), None);
		assert_eq!(fallback_outcome(principal, [&first]).unwrap(), Some(FallbackOutcome::Pending));
		first.confirmed_block = Some(BlockRef { height: BlockHeight::new(1), hash: bitcoin::BlockHash::all_zeros() });
		assert_eq!(fallback_outcome(principal, [&first]).unwrap(), Some(FallbackOutcome::Pending));
		let mut second = FallbackClaim {
			vtxo_id: first.vtxo_id, source_vtxo_ids: BTreeSet::new(), owner_mailbox: key,
			binding_source: None, user_pubkey: key,
			gross: Amount::from_sat(60_000), payout: Amount::ZERO,
			confirmed_block: None, user_claimed: false,
		};
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Pending));
		second.confirmed_block = first.confirmed_block;
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Spent));
		first.payout = Amount::ZERO;
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Forfeited));
		assert!(fallback_outcome(Amount::from_sat(99_999), [&first, &second]).is_err());
		first.confirmed_block = None;
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Pending));
		first.user_claimed = true;
		assert_eq!(fallback_outcome(principal, [&first]).unwrap(), Some(FallbackOutcome::Pending));
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Spent));
		second.confirmed_block = None;
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Pending));
		second.user_claimed = true;
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Spent));
		first.user_claimed = false;
		assert_eq!(fallback_outcome(principal, [&first, &second]).unwrap(), Some(FallbackOutcome::Pending));
	}

	#[test]
	fn receive_timeout_keeps_collected_and_inflight_grants() {
		assert!(receive_timeout_allowed(HtlcsReady, None, None, false));
		assert!(!receive_timeout_allowed(HtlcsReady, None, None, true));
		for known in [false, true] {
			assert!(receive_timeout_allowed(Canceled, None, None, known));
			assert!(receive_timeout_allowed(Canceled, Some(Revoked), None, known));
			for status in [Created, Accepted, HtlcsReady, Settled, Canceled] {
				assert!(!receive_timeout_allowed(status, Some(Fulfilled), None, known));
				assert!(!receive_timeout_allowed(status, None, Some(Fulfilled), known));
			}
			for status in [Created, Accepted, Settled] {
				assert!(!receive_timeout_allowed(status, None, None, known));
			}
		}
	}

}
