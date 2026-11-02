use std::time::Duration;

use bitcoin::{Amount, NetworkKind};
use bitcoin::bip32::{Xpriv, Xpub};
use bitcoin::secp256k1::{Keypair, PublicKey};

use ark::{ProtocolEncoding, ServerVtxo, Vtxo, VtxoId, SECP};
use ark::attestations::VtxoStatusAttestation;
use ark::recovery::XpubRecoveryAuthorization;
use ark::test_util::dummy::DummyTestVtxoSpec;
use ark::vtxo::Full;
use server::database::Db;
use server_rpc::{protos, RequestExt};

use ark_testing::TestContext;

fn dummy_xpriv(network: NetworkKind) -> Xpriv {
	Xpriv::new_master(network, &[42u8; 32]).unwrap()
}

fn auth(xpriv: &Xpriv, server_pubkey: PublicKey) -> Vec<u8> {
	let expiry = chrono::Local::now() + Duration::from_secs(60);
	XpubRecoveryAuthorization::new(xpriv, server_pubkey, expiry).serialize()
}

fn xpub_request(
	xpriv: &Xpriv,
	server_pubkey: PublicKey,
	start_index: u32,
) -> protos::RecoverVtxosByXpubRequest {
	protos::RecoverVtxosByXpubRequest {
		xpub: Xpub::from_priv(&SECP, xpriv).encode().to_vec(),
		start_index,
		authorization: auth(xpriv, server_pubkey),
		cursor_vtxo_id: None,
	}
}

include!("recovery_fallback.rs");
