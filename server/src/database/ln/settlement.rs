//! Source-specific settlement from the existing preimage WAL and terminal facts.

use anyhow::Context;
use ark::VtxoId;
use ark::lightning::PaymentHash;

use crate::database::Tx;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceSettlement {
	NotSettled,
	Funded,
	Unknown,
}
impl Tx<'_> {
	/// Settlement and refund decisions take validity, sorted sources, then the
	/// settlement advisory lock. A real preimage is retained even during a reorg.
	pub async fn lock_settlement_sources(&self, hash: PaymentHash) -> anyhow::Result<()> {
		self.query("SELECT height FROM watchmand_block ORDER BY height ASC LIMIT 1 FOR SHARE", &[]).await?;
		self.query("SELECT v.vtxo_id FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id \
			WHERE h.payment_hash=$1 ORDER BY v.vtxo_id FOR UPDATE OF v", &[&hash.to_string()]).await?;
		Ok(())
	}

	async fn settlement_attempt_sources(&self, hash: PaymentHash, attempt: i64) -> anyhow::Result<Option<Vec<VtxoId>>> {
		let Some(row) = self.query_opt("SELECT payment_hash FROM lightning_payment_attempt WHERE id=$1", &[&attempt]).await?
			else { return Ok(None); };
		if row.get::<_, &str>(0) != hash.to_string() { return Ok(None); }
		let rows = self.query("SELECT l.vtxo_id,h.payment_hash,h.direction::TEXT FROM lightning_payment_attempt_htlc_vtxo l \
			LEFT JOIN vtxo v ON v.vtxo_id=l.vtxo_id LEFT JOIN htlc_vtxo h ON h.id=v.id \
			WHERE l.lightning_payment_attempt_id=$1 ORDER BY l.vtxo_id", &[&attempt]).await?;
		if rows.is_empty() { return Ok(None); }
		let mut sources = Vec::with_capacity(rows.len());
		for row in rows {
			if row.get::<_, Option<&str>>("payment_hash") != Some(hash.to_string().as_str())
				|| row.get::<_, Option<&str>>("direction") != Some("incoming") { return Ok(None); }
			sources.push(row.get::<_, &str>("vtxo_id").parse()?);
		}
		Ok(Some(sources))
	}

	pub(crate) fn warn_unknown_settlement(&self, source: VtxoId) {
		self.warn_settlement_integrity(source, "settlement_provenance_unknown");
	}

	pub(crate) fn warn_settlement_integrity(&self, source: VtxoId, reason: &str) {
		self.observe_expiry_hold(source, reason);
		tracing::error!(vtxo_id = %source, reason, "Settlement integrity is unresolved; holding principal");
	}

	/// A matching preimage alone proves neither which source paid nor that an
	/// ambiguous source may be refunded. Require this source's existing terminal
	/// facts as well; a missing WAL beside terminal facts remains unknown.
	pub async fn source_settlement(&self, hash: PaymentHash, source: VtxoId) -> anyhow::Result<SourceSettlement> {
		let preimage = self.get_htlc_settlement_by_payment_hash(hash).await?;
		if let Some(preimage) = preimage {
			ensure!(preimage.compute_payment_hash() == hash, "settlement preimage differs from its payment hash");
		}
		let Some(row) = self.query_opt("SELECT h.payment_hash,h.direction::TEXT, \
			((COALESCE(h.chain_resolution::TEXT,h.offchain_resolution::TEXT,'')='fulfilled' \
				AND (h.chain_resolution IS NULL OR (v.onchain_spent_txid IS NOT NULL \
					AND h.chain_resolution_height IS NOT NULL AND v.onchain_spent_height IS NOT NULL \
					AND h.chain_resolution_height=v.onchain_spent_height))) \
			OR (h.direction='incoming' AND EXISTS(SELECT 1 FROM lightning_payment_attempt a \
				JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id \
				WHERE l.vtxo_id=v.vtxo_id AND a.payment_hash=$2 AND a.status='succeeded')) \
			OR (h.direction='outgoing' AND EXISTS(SELECT 1 FROM lightning_htlc_subscription s \
				WHERE s.id=v.lightning_htlc_subscription_id AND s.payment_hash=$2 AND s.status='settled'))) AS funded, \
			EXISTS(SELECT 1 FROM lightning_payment_attempt a \
				WHERE a.payment_hash=$2 AND a.status='succeeded') AS hash_succeeded, \
			EXISTS(SELECT 1 FROM lightning_payment_attempt a \
				JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id \
				WHERE l.vtxo_id=v.vtxo_id AND a.payment_hash=$2 \
				AND a.status IN ('requested','submitted')) AS source_open \
			FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id WHERE v.vtxo_id=$1",
			&[&source.to_string(), &hash.to_string()]).await? else { return Ok(SourceSettlement::Unknown); };
		if row.get::<_, &str>("payment_hash") != hash.to_string()
			|| !matches!(row.get::<_, &str>("direction"), "incoming" | "outgoing") {
			return Ok(SourceSettlement::Unknown);
		}
		Ok(match (preimage.is_some(), row.get::<_, bool>("funded"),
			row.get::<_, bool>("hash_succeeded"), row.get::<_, bool>("source_open"),
			row.get::<_, &str>("direction")) {
			(true, true, _, _, _) => SourceSettlement::Funded,
			(false, false, _, _, _) | (true, false, true, false, "incoming") => SourceSettlement::NotSettled,
			_ => SourceSettlement::Unknown,
		})
	}

	/// Call under the source and settlement locks in the failure transaction.
	/// A native failure cannot release a source paid by another observation,
	/// or whose settlement facts are incomplete.
	pub async fn ensure_attempt_sources_allow_failure(&self, attempt: &super::LightningPaymentAttempt)
		-> anyhow::Result<()>
	{
		let sources = self.settlement_attempt_sources(attempt.payment_hash, attempt.id).await?
			.context("payment attempt source provenance is unavailable")?;
		for source in sources {
			match self.source_settlement(attempt.payment_hash, source).await? {
				SourceSettlement::NotSettled => {},
				SourceSettlement::Funded | SourceSettlement::Unknown => {
					return badarg!("source settlement does not permit failure of payment attempt {}", attempt.id);
				},
			}
		}
		Ok(())
	}
}
