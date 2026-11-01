use bitcoin::secp256k1::{Keypair, rand::thread_rng};
use server_rpc::{protos, RequestExt};
use ark_testing::{Captaind, TestContext};

/// The inherited admin-fail gate is observation-only: the operator supplies
/// an ID, never a requested outcome. Authority is the same private listener.
#[tokio::test]
async fn fail_attempt_requires_admin_auth() {
	let ctx = TestContext::new("server/fail_attempt_requires_admin_auth").await;
	let srv = ctx.captaind("server").no_vtxo_pool().create().await;
	let request = || {
		let mut request = tonic::Request::new(protos::scuttle::ReconcileLightningAttemptRequest { attempt_id: i64::MAX });
		request.set_pver(server_rpc::pver::PROTOCOL_VERSION_SCUTTLE);
		request
	};
	let mut public = server_rpc::admin::ScuttleServiceClient::connect(srv.ark_url()).await.unwrap();
	assert_eq!(public.reconcile_lightning_attempt(request()).await.unwrap_err().code(), tonic::Code::Unimplemented);
	let address = srv.config().rpc.admin_address.unwrap().to_string();
	let mut admin = server_rpc::admin::ScuttleServiceClient::connect(format!("http://{address}")).await.unwrap();
	let missing = protos::scuttle::ReconcileLightningAttemptRequest { attempt_id: i64::MAX };
	assert_eq!(admin.reconcile_lightning_attempt(missing).await.unwrap_err().code(), tonic::Code::InvalidArgument);
	let mut old = request();
	old.set_pver(server_rpc::pver::PROTOCOL_VERSION_SCUTTLE - 1);
	assert_eq!(admin.reconcile_lightning_attempt(old).await.unwrap_err().code(), tonic::Code::InvalidArgument);
	assert_eq!(admin.reconcile_lightning_attempt(request()).await.unwrap_err().code(), tonic::Code::NotFound);
	let output = Captaind::base_cmd().env("RUST_LOG", "off")
		.args(["rpc", "--addr", &address, "reconcile-lightning-attempt", &i64::MAX.to_string()]).output().await.unwrap();
	assert!(!output.status.success());
	assert!(String::from_utf8_lossy(&output.stderr).contains("unknown Lightning payment attempt"));
}

/// The fork epoch is required before operations, and operator methods are not public.
#[tokio::test]
async fn scuttle_wire_epoch_and_operator_listener_are_explicit() {
	let ctx = TestContext::new("server/scuttle_wire_epoch_and_operator_listener").await;
	let srv = ctx.captaind("server").create().await;
	let mut public = server_rpc::ArkServiceClient::connect(srv.ark_url()).await.unwrap();
	let handshake = public.handshake(protos::HandshakeRequest {
		bark_version: Some(server_rpc::BARK_CRATE_VERSION.to_owned()),
	}).await.unwrap().into_inner();
	let version = server_rpc::pver::PROTOCOL_VERSION_SCUTTLE;
	assert_eq!(handshake.min_protocol_version, version);
	assert_eq!(handshake.max_protocol_version, version);
	let missing = public.get_offboard_fee_rate(protos::Empty {}).await.unwrap_err();
	assert_eq!(missing.code(), tonic::Code::InvalidArgument);
	assert!(missing.message().contains("pver"));
	for old in [4, 5, 6, 7, version - 1, version + 1] {
		let mut request = tonic::Request::new(protos::Empty {});
		request.set_pver(old);
		let error = public.get_offboard_fee_rate(request).await.unwrap_err();
		assert_eq!(error.code(), tonic::Code::InvalidArgument);
		assert!(error.message().contains("unsupported Scuttle protocol"));
	}
	let mut request = tonic::Request::new(protos::Empty {});
	request.set_pver(version);
	public.get_offboard_fee_rate(request).await.unwrap();
	// A missing current proof remains refused independently of retired tags.
	let key = Keypair::new(&ark::SECP, &mut thread_rng());
	let mut request = tonic::Request::new(protos::BoardCosignRequest { retired_owner_proof: None,
		user_pubkey: key.public_key().serialize().to_vec(),
		..Default::default()
	});
	request.set_pver(version);
	let missing = public.request_board_cosign(request).await.unwrap_err();
	assert_eq!(missing.code(), tonic::Code::InvalidArgument);
	assert_eq!(missing.metadata().get("abandon-reason").unwrap().to_str().unwrap(), "owner_proof_required");

	let mailbox_key = Keypair::new(&ark::SECP, &mut thread_rng());
	let proof = ark::fallback::OwnerLink::new_joint(bitcoin::Network::Regtest,
		srv.mailbox_keypair().public_key(), &mailbox_key, &key).unwrap();
	for retired in [vec![], vec![1, 2, 3]] {
		let mut board = tonic::Request::new(protos::BoardCosignRequest {
			user_pubkey: key.public_key().serialize().to_vec(), owner_proof: Some(proof.into()),
			retired_owner_proof: Some(retired.clone()), ..Default::default()
		});
		board.set_pver(version);
		let error = public.request_board_cosign(board).await.unwrap_err();
		assert_eq!(error.code(), tonic::Code::InvalidArgument);
		assert_eq!(error.message(), "retired owner proof field");
		let mut claim = tonic::Request::new(protos::PrepareLightningReceiveClaimRequest {
			owner_proof: Some(proof.into()), retired_owner_proof: Some(retired.clone()),
			..Default::default()
		});
		claim.set_pver(version);
		let error = public.prepare_lightning_receive_claim(claim).await.unwrap_err();
		assert_eq!(error.code(), tonic::Code::InvalidArgument);
		assert_eq!(error.message(), "retired owner proof field");
		for isolated in [false, true] {
			let output = protos::ArkoorDestination {
				owner_proof: Some(proof.into()), retired_owner_proof: Some(retired.clone()),
				..Default::default()
			};
			let mut part = protos::ArkoorCosignRequest::default();
			if isolated { part.isolated_outputs.push(output); } else { part.outputs.push(output); }
			let mut request = tonic::Request::new(protos::ArkoorPackageCosignRequest { parts: vec![part] });
			request.set_pver(version);
			let error = public.request_arkoor_cosign(request).await.unwrap_err();
			assert_eq!(error.code(), tonic::Code::InvalidArgument);
			assert!(error.message().contains("retired owner proof field"));
		}
	}

	let mut mailbox = server_rpc::mailbox::MailboxServiceClient::connect(srv.ark_url()).await.unwrap();
	let error = mailbox.read_mailbox(protos::mailbox_server::MailboxRequest::default()).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument);
	assert!(error.message().contains("pver"));
	let mut request = tonic::Request::new(protos::mailbox_server::MailboxRequest::default());
	request.set_pver(7);
	let error = mailbox.read_mailbox(request).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument);
	assert!(error.message().contains("unsupported Scuttle protocol"));

	let mut public_operator = server_rpc::admin::ScuttleServiceClient::connect(srv.ark_url()).await.unwrap();
	let mut request = tonic::Request::new(protos::scuttle::ReconcileLightningAttemptRequest { attempt_id: 0 });
	request.set_pver(version);
	assert_eq!(public_operator.reconcile_lightning_attempt(request).await.unwrap_err().code(), tonic::Code::Unimplemented);
	let address = format!("http://{}", srv.config().rpc.admin_address.unwrap());
	let mut operator = server_rpc::admin::ScuttleServiceClient::connect(address).await.unwrap();
	let error = operator.reconcile_lightning_attempt(protos::scuttle::ReconcileLightningAttemptRequest { attempt_id: 0 }).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument);
	assert!(error.message().contains("pver"));
	let mut old = tonic::Request::new(protos::scuttle::ReconcileLightningAttemptRequest { attempt_id: 0 });
	old.set_pver(version - 1);
	assert_eq!(operator.reconcile_lightning_attempt(old).await.unwrap_err().code(), tonic::Code::InvalidArgument);
	let mut request = tonic::Request::new(protos::scuttle::ReconcileLightningAttemptRequest { attempt_id: 0 });
	request.set_pver(version);
	let error = operator.reconcile_lightning_attempt(request).await.unwrap_err();
	assert_eq!(error.code(), tonic::Code::InvalidArgument);
	assert_eq!(error.message(), "attempt_id must be positive");
}
