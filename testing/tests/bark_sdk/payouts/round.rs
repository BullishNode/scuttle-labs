use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::path::PathBuf;
use std::time::Duration;

use bitcoin::hashes::Hash;
use tokio_stream::StreamExt;

use ark::{ProtocolEncoding, Vtxo, VtxoId};
use ark::attestations::RoundParticipationStatusAttestation;
use ark::vtxo::Full;
use ark::tree::signed::UnlockHash;
use ark::rounds::RoundEvent;
use bark::{OpenWalletArgs, Wallet, WalletSeed};
use bark::movement::{MovementId, MovementStatus};
use bark::onchain::OnchainWallet;
use bark::persist::BarkPersister;
use bark::persist::adaptor::StorageAdaptorWrapper;
use bark::persist::adaptor::filestore::FileStorageAdaptor;
use bark::persist::models::{RoundStateId, SerdeRoundState};
use bark::round::{RoundFlowKind, RoundStatus};
use bark::subsystem::RoundMovement;
use bark::vtxo::{VtxoLockHolder, VtxoState};
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin::{Amount, Network, OutPoint, Transaction};
use bitcoin_ext::TxOutExt;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server_log::RoundFinished;
use server_rpc::protos;

use ark_testing::{btc, sat, secs, TestContext};
use ark_testing::constants::{BOARD_CONFIRMATIONS, ROUND_CONFIRMATIONS};
use ark_testing::daemon::captaind::{self, ArkClient, Captaind};
use ark_testing::util::{FutureExt, poll_interval};

#[derive(Clone, Copy, Default)]
enum CancellationStatusFault {
	#[default]
	Pass,
	IssuedThenUnknown,
	Generic,
	MissingReason,
	MissingHash,
	WrongReason,
	MalformedHash,
	WrongHash,
	MultipleHashes,
	DuplicateHash,
	DuplicateReason,
	Unavailable,
}

#[derive(Clone, Default)]
struct RefreshTransport {
	status_fault: Arc<Mutex<CancellationStatusFault>>,
	status_steps: Arc<AtomicUsize>,
	status_faults: Arc<AtomicUsize>,
	reject_nonces: Arc<AtomicBool>,
	nonce_requests: Arc<AtomicUsize>,
	accepted_forfeits: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for RefreshTransport {
	async fn request_forfeit_nonces(&self, upstream: &mut ArkClient, request: protos::ForfeitNoncesRequest)
		-> Result<protos::ForfeitNoncesResponse, tonic::Status>
	{
		self.nonce_requests.fetch_add(1, Ordering::SeqCst);
		if self.reject_nonces.load(Ordering::SeqCst) {
			return Err(tonic::Status::unavailable("nonce transport unavailable before forwarding"));
		}
		Ok(upstream.request_forfeit_nonces(request).await?.into_inner())
	}

	async fn forfeit_vtxos(&self, upstream: &mut ArkClient, request: protos::ForfeitVtxosRequest)
		-> Result<protos::ForfeitVtxosResponse, tonic::Status>
	{
		let response = upstream.forfeit_vtxos(request).await?.into_inner();
		self.accepted_forfeits.fetch_add(1, Ordering::SeqCst);
		Ok(response)
	}

	async fn round_participation_status(&self, upstream: &mut ArkClient, request: protos::RoundParticipationStatusRequest)
		-> Result<protos::RoundParticipationStatusResponse, tonic::Status>
	{
		let hash = UnlockHash::from_slice(&request.unlock_hash).unwrap();
		let result = upstream.round_participation_status(request).await;
		let fault = *self.status_fault.lock().unwrap();
		if matches!(fault, CancellationStatusFault::Pass) {
			return Ok(result?.into_inner());
		}
		if matches!(fault, CancellationStatusFault::IssuedThenUnknown) {
			let response = result?.into_inner();
			assert_eq!(response.status, protos::RoundParticipationStatus::RoundPartIssued as i32);
			if self.status_steps.fetch_add(1, Ordering::SeqCst) == 0 { return Ok(response); }
			self.status_faults.fetch_add(1, Ordering::SeqCst);
			let mut error = tonic::Status::not_found("participation not found during second status request");
			error.metadata_mut().insert("identifiers", hash.to_string().parse().unwrap());
			return Err(error);
		}
		// Change only the transport reply to a real durable cancellation.
		let original = match result {
			Err(error) => error,
			Ok(_) => panic!("negative metadata cases require a real cancellation tombstone"),
		};
		assert_eq!(original.code(), tonic::Code::NotFound);
		assert_eq!(original.metadata().get("abandon-reason").unwrap().to_str().unwrap(), "round_cancelled_before_forfeit");
		assert_eq!(original.metadata().get("identifiers").unwrap().to_str().unwrap(), hash.to_string());
		self.status_faults.fetch_add(1, Ordering::SeqCst);
		let mut metadata = original.metadata().clone();
		let mut wrong = hash.to_byte_array();
		wrong[0] ^= 1;
		let wrong = UnlockHash::from_byte_array(wrong).to_string();
		match fault {
			CancellationStatusFault::Generic => return Err(tonic::Status::not_found("round_cancelled_before_forfeit")),
			CancellationStatusFault::MissingReason => { metadata.remove("abandon-reason"); },
			CancellationStatusFault::MissingHash => { metadata.remove("identifiers"); },
			CancellationStatusFault::WrongReason => { metadata.insert("abandon-reason", "unknown_round".parse().unwrap()); },
			CancellationStatusFault::MalformedHash => { metadata.insert("identifiers", "not-a-hash".parse().unwrap()); },
			CancellationStatusFault::WrongHash => { metadata.insert("identifiers", wrong.parse().unwrap()); },
			CancellationStatusFault::MultipleHashes => { metadata.insert("identifiers", format!("{hash},{wrong}").parse().unwrap()); },
			CancellationStatusFault::DuplicateHash => { metadata.append("identifiers", hash.to_string().parse().unwrap()); },
			CancellationStatusFault::DuplicateReason => { metadata.append("abandon-reason", "round_cancelled_before_forfeit".parse().unwrap()); },
			CancellationStatusFault::Unavailable => return Err(tonic::Status::with_metadata(tonic::Code::Unavailable, "status transport unavailable", metadata)),
			CancellationStatusFault::Pass | CancellationStatusFault::IssuedThenUnknown => unreachable!(),
		}
		Err(tonic::Status::with_metadata(tonic::Code::NotFound, "cancellation metadata changed in transit", metadata))
	}
}

type RefreshDatabase = StorageAdaptorWrapper<FileStorageAdaptor>;

struct IssuedRefresh {
	wallet: Wallet,
	proxy: captaind::proxy::ArkRpcProxyServer,
	database: Arc<RefreshDatabase>,
	storage_path: PathBuf,
	mnemonic: bip39::Mnemonic,
	source: Vtxo<Full>,
	state_id: RoundStateId,
	movement: MovementId,
	unlock_hash: UnlockHash,
	sweep_at: BlockHeight,
	first_height: u64,
}

include!("round_fallback.rs");
