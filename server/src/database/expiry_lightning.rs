//! Read Lightning ownership from existing source links and upstream attempt status.

use ark::VtxoId;
use ark::lightning::PaymentHash;
use bitcoin_ext::BlockHeight;

use super::Tx;
use super::htlc_vtxo::{self, HtlcDirection, HtlcVtxo};
use super::ln::LightningPaymentStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LightningSourceState {
	Unattempted,
	Failed,
	Pending,
	Discharged,
}

/// Fold the attempts of one source, each with whether the receive it was
/// initiated against is canceled.
///
/// An open attempt on a canceled receive counts as failed: a canceled receive
/// refuses every claim, so that attempt can never succeed, and nothing fails
/// it if the server stopped between canceling the receive and failing it.
fn source_state(attempts: impl IntoIterator<Item = (LightningPaymentStatus, bool)>) -> LightningSourceState {
	let mut state = LightningSourceState::Unattempted;
	for (status, receive_canceled) in attempts {
		match status {
			LightningPaymentStatus::Succeeded => return LightningSourceState::Discharged,
			LightningPaymentStatus::Requested | LightningPaymentStatus::Submitted if !receive_canceled => {
				state = LightningSourceState::Pending;
			},
			_ if state == LightningSourceState::Unattempted => state = LightningSourceState::Failed,
			_ => {},
		}
	}
	state
}

impl Tx<'_> {
	pub(crate) async fn lightning_source_state(
		&self, source: VtxoId, hash: PaymentHash,
	) -> anyhow::Result<LightningSourceState> {
		let rows = self.query("SELECT a.status, COALESCE(s.status='canceled', FALSE) AS receive_canceled \
			FROM lightning_payment_attempt a \
			JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id \
			LEFT JOIN lightning_htlc_subscription s ON s.id=a.lightning_htlc_subscription_id \
			WHERE l.vtxo_id=$1 AND a.payment_hash=$2", &[&source.to_string(), &hash.to_string()]).await?;
		Ok(source_state(rows.into_iter().map(|row| (row.get("status"), row.get("receive_canceled")))))
	}

	/// Whether the receive a failed intra-Ark source paid into can no longer
	/// be claimed: it is canceled, and every grant it issued is past its
	/// expiry, six blocks deep if it was reclaimed on-chain.
	///
	/// `None` if the source funded no receive that issued grants. `htlcs` are
	/// all the HTLCs of the payment hash, which carries at most one receive.
	pub(crate) async fn internal_receive_closed(
		&self, source: VtxoId, hash: PaymentHash, htlcs: &[HtlcVtxo], tip: BlockHeight,
	) -> anyhow::Result<Option<bool>> {
		let row = self.query_one("SELECT COUNT(*) AS linked, \
			COUNT(*) FILTER (WHERE s.status<>'canceled') AS open \
			FROM lightning_payment_attempt a \
			JOIN lightning_payment_attempt_htlc_vtxo l ON l.lightning_payment_attempt_id=a.id \
			JOIN lightning_htlc_subscription s ON s.id=a.lightning_htlc_subscription_id \
			WHERE l.vtxo_id=$1 AND a.payment_hash=$2", &[&source.to_string(), &hash.to_string()]).await?;
		if row.get::<_, i64>("linked") == 0 || !htlcs.iter().any(|h| h.direction() == HtlcDirection::Outgoing) {
			return Ok(None);
		}
		Ok(Some(row.get::<_, i64>("open") == 0
			&& htlc_vtxo::payout_htlc_recvs_expired(htlcs, tip, Some(tip))))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ownership_uses_only_existing_attempt_states() {
		use LightningPaymentStatus::*;
		let open = |statuses: &[LightningPaymentStatus]| source_state(statuses.iter().map(|s| (*s, false)));
		assert_eq!(open(&[]), LightningSourceState::Unattempted);
		assert_eq!(open(&[Failed]), LightningSourceState::Failed);
		assert_eq!(open(&[Failed, Submitted]), LightningSourceState::Pending);
		assert_eq!(open(&[Requested, Failed]), LightningSourceState::Pending);
		assert_eq!(open(&[Failed, Succeeded]), LightningSourceState::Discharged);
		// An open attempt on a canceled receive can never succeed.
		assert_eq!(source_state([(Requested, true)]), LightningSourceState::Failed);
		assert_eq!(source_state([(Requested, true), (Submitted, false)]), LightningSourceState::Pending);
		assert_eq!(source_state([(Succeeded, true)]), LightningSourceState::Discharged);
	}
}
