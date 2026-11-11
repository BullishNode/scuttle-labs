use server_rpc::{protos, RequestExt};
use ark_testing::TestContext;
use server::database::Db;

/// Payout-on operations use the upstream numeric version and an explicit
/// Scuttle label.
#[tokio::test]
async fn scuttle_wire_epoch_and_operator_listener_are_explicit() {
	let ctx = TestContext::new("server/scuttle_wire_epoch_and_operator_listener").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let mut public = server_rpc::ArkServiceClient::connect(srv.ark_url()).await.unwrap();
	let handshake = public.handshake(protos::HandshakeRequest {
		bark_version: Some(server_rpc::BARK_CRATE_VERSION.to_owned()),
	}).await.unwrap().into_inner();
	let version = server_rpc::pver::PROTOCOL_VERSION_HASHLOCK_CLAUSES;
	assert_eq!(handshake.min_protocol_version, version);
	assert_eq!(handshake.max_protocol_version, version);
	assert_eq!(handshake.protocol_label.as_deref(), Some("scuttle-1"));
	let missing = public.get_offboard_fee_rate(protos::Empty {}).await.unwrap_err();
	assert_eq!(missing.code(), tonic::Code::InvalidArgument);
	assert!(missing.message().contains("pver"));
	for old in [4, 6, 7] {
		let mut request = tonic::Request::new(protos::Empty {});
		request.set_pver(old);
		let error = public.get_offboard_fee_rate(request).await.unwrap_err();
		assert_eq!(error.code(), tonic::Code::InvalidArgument);
		assert!(error.message().contains("unsupported protocol"));
	}
	let mut request = tonic::Request::new(protos::Empty {});
	request.set_pver(version);
	public.get_offboard_fee_rate(request).await.unwrap();
	let mut mailbox = server_rpc::mailbox::MailboxServiceClient::connect(srv.ark_url()).await.unwrap();
	let error = mailbox.read_mailbox(protos::mailbox_server::MailboxRequest::default()).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument);
	assert!(error.message().contains("pver"));
	let mut request = tonic::Request::new(protos::mailbox_server::MailboxRequest::default());
	request.set_pver(7);
	let error = mailbox.read_mailbox(request).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument);
	assert!(error.message().contains("unsupported protocol"));

	// The next process configuration cannot change this store's payout mode.
	let server_pubkey = srv.ark_info().await.server_pubkey;
	let postgres = srv.config().postgres.clone();
	let db = Db::connect(&postgres).await.unwrap();
	let mode = db.read(async |tx| Ok(tx.query_one(
		"SELECT expiry_payouts FROM server_mode WHERE singleton", &[],
	).await?.get::<_, bool>(0))).await.unwrap();
	assert!(mode);
	srv.stop().await.unwrap();
	let stderr_path = srv.config().data_dir.join("stderr.log");
	let stderr_offset = tokio::fs::metadata(&stderr_path).await.unwrap().len() as usize;
	srv.config_mut().expiry_payouts = false;
	let error = srv.start().await.expect_err("an ON store must refuse an OFF configuration");
	let stderr = tokio::fs::read(&stderr_path).await.unwrap();
	let refusal = std::str::from_utf8(&stderr[stderr_offset..]).unwrap();
	assert!(refusal.contains("expiry_payouts differs from the mode fixed at database creation"),
		"startup must refuse the mode change, not fail for an unrelated reason: {error:#}");
	let unchanged = db.read(async |tx| Ok(tx.query_one(
		"SELECT expiry_payouts FROM server_mode WHERE singleton", &[],
	).await?.get::<_, bool>(0))).await.unwrap();
	assert_eq!(unchanged, mode);
	srv.config_mut().expiry_payouts = true;
	srv.start().await.unwrap();
	assert_eq!(srv.ark_info().await.server_pubkey, server_pubkey);
	let mut restored = server_rpc::ArkServiceClient::connect(srv.ark_url()).await.unwrap();
	let restored = restored.handshake(protos::HandshakeRequest {
		bark_version: Some(server_rpc::BARK_CRATE_VERSION.to_owned()),
	}).await.unwrap().into_inner();
	assert_eq!((restored.min_protocol_version, restored.max_protocol_version), (version, version));

	// A separate ordinary store keeps the unmodified public version range.
	let ordinary = ctx.captaind("ordinary").no_vtxo_pool().create().await;
	assert!(!ordinary.config().expiry_payouts);
	let mut public = server_rpc::ArkServiceClient::connect(ordinary.ark_url()).await.unwrap();
	let handshake = public.handshake(protos::HandshakeRequest {
		bark_version: Some(server_rpc::BARK_CRATE_VERSION.to_owned()),
	}).await.unwrap().into_inner();
	assert_eq!((handshake.min_protocol_version, handshake.max_protocol_version), (4, 5));
	for version in [4, 5] {
		let mut request = tonic::Request::new(protos::Empty {});
		request.set_pver(version);
		public.get_offboard_fee_rate(request).await.unwrap();
	}
}
