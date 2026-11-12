// Reply-loss observer shared by both targets.

#[derive(Clone)]
struct HoldSuccessfulReceiveClaim {
	hold: Arc<AtomicBool>,
	arrived: Arc<tokio::sync::Notify>,
	requests: Arc<AtomicUsize>,
	hold_before: bool,
	release_before: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for HoldSuccessfulReceiveClaim {
	async fn claim_lightning_receive(
		&self, upstream: &mut ArkClient, request: protos::ClaimLightningReceiveRequest,
	) -> Result<protos::ArkoorPackageCosignResponse, tonic::Status> {
		self.requests.fetch_add(1, Ordering::SeqCst);
		if self.hold_before && self.hold.load(Ordering::SeqCst) {
			self.arrived.notify_one();
			self.release_before.notified().await;
		}
		let response = upstream.claim_lightning_receive(request).await?.into_inner();
		if self.hold.swap(false, Ordering::SeqCst) {
			self.arrived.notify_one();
			std::future::pending::<()>().await;
		}
		Ok(response)
	}
}
