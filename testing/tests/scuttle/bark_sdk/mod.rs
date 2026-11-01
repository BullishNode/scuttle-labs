mod arkoor;
mod balance;
mod fallback;
mod fallback_adoption;
mod fallback_external;
mod fallback_protection;
mod lightning;
mod lightning_receive_binding;
mod round;

/// Projection of authenticated statuses on actual retained coins; no ancestry or payout ledger.
fn fallback_status_ids(coins: Vec<bark::WalletVtxo>, terminal: bool) -> std::collections::BTreeSet<ark::VtxoId> {
	coins.into_iter().filter(|coin| coin.fallback_status.is_some_and(|status|
		(status != bark::vtxo::FallbackStatus::Pending) == terminal))
		.map(|coin| coin.id()).collect()
}
