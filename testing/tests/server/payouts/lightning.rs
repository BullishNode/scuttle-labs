use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bitcoin::{Amount, Network, OutPoint, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin::secp256k1::Keypair;

use ark::{ProtocolEncoding, Vtxo, VtxoPolicy, SECP};
use ark::lightning::PaymentHash;
use ark::fallback::OwnerLink;
use ark::mailbox::MailboxIdentifier;
use bitcoin_ext::{BlockDelta, BlockHeight, TxOutExt};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use ark::vtxo::Full;
use bark::Wallet;
use bark::lightning_invoice::Bolt11Invoice;
use server_rpc::protos;
use server::database::Db;
use server::vtxopool::VtxoTarget;

use ark_testing::{Captaind, TestContext, btc, sat};
use ark_testing::constants::BOARD_CONFIRMATIONS;
use ark_testing::daemon::captaind::{self, ArkClient};
use ark_testing::util::{poll_interval, FutureExt};

use cln_rpc::plugins::hold;

/// Asserts that every unspent entry in `vtxo_pool` (`spent_at IS NULL`)
/// references a `vtxo` row with `spend_state = 'pool'`.
async fn htlc_send_vtxo_ids(
	client: &Wallet,
	payment_hash: ark::lightning::PaymentHash,
) -> Vec<ark::VtxoId> {
	let ids = client.all_vtxos().await.unwrap()
		.into_iter()
		.filter_map(|wv| {
			let pol = wv.vtxo.policy().as_server_htlc_send()?;
			(pol.payment_hash == payment_hash).then(|| wv.vtxo.id())
		})
		.collect::<Vec<ark::VtxoId>>();
	assert!(!ids.is_empty(), "the payment should have left HTLC vtxos in the wallet");
	ids
}

/// Build a "claim all" revocation request from the HTLC-send vtxos `client`
/// still holds for `payment_hash`, send it straight to the server, and return
/// the error the server responds with (the caller asserts on it).
async fn request_htlc_revocation(
	srv: &Captaind,
	client: &Wallet,
	payment_hash: ark::lightning::PaymentHash,
) -> tonic::Status {
	try_request_htlc_revocation(srv, client, payment_hash).await
		.expect_err("server should refuse revocation for a settled payment")
}

/// Like [request_htlc_revocation], but returns whether the server cosigned.
async fn try_request_htlc_revocation(
	srv: &Captaind,
	client: &Wallet,
	payment_hash: ark::lightning::PaymentHash,
) -> Result<(), tonic::Status> {
	let htlc_vtxo_ids = htlc_send_vtxo_ids(client, payment_hash).await;

	let mut htlc_vtxos = Vec::with_capacity(htlc_vtxo_ids.len());
	let mut keypairs = Vec::with_capacity(htlc_vtxo_ids.len());
	for id in &htlc_vtxo_ids {
		let vtxo = client.get_full_vtxo(*id).await.unwrap();
		keypairs.push(client.get_vtxo_key(&vtxo).await.unwrap());
		htlc_vtxos.push(vtxo);
	}
	revoke_htlc_vtxos(srv, htlc_vtxos, &keypairs).await
}

/// Send a "claim all" revocation of `htlc_vtxos`, signed with `keypairs`,
/// straight to the server.
async fn revoke_htlc_vtxos(
	srv: &Captaind,
	htlc_vtxos: Vec<Vtxo<Full>>,
	keypairs: &[Keypair],
) -> Result<(), tonic::Status> {
	// Any pubkey works for the revocation output; the tests never claim it.
	let revocation_key = Keypair::new(&SECP, &mut bip39::rand::thread_rng());
	let revocation_pubkey = revocation_key.public_key();
	let owner_proof = super::test_owner_link(srv, &revocation_key).await;
	let builder = ark::arkoor::package::ArkoorPackageBuilder::new_claim_all_with_checkpoints(
		htlc_vtxos,
		ark::VtxoPolicy::new_pubkey(revocation_pubkey),
	).unwrap().generate_user_nonces(keypairs).unwrap();

	let mut cosign_request = protos::ArkoorPackageCosignRequest::from(builder.cosign_request());
	for output in cosign_request.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		output.owner_proof = Some(owner_proof.into());
	}
	let cosign_request = super::signed_package(srv, &[], cosign_request).await;

	let mut srv_rpc = srv.get_public_rpc().await;
	srv_rpc.request_lightning_pay_htlc_revocation(cosign_request).await.map(|_| ())
}

/// Verify that the server extracts preimages from on-chain HTLC spends
/// and uses them to settle invoices.
///
/// The proxy blocks cooperative settlement, so bark explicitly exits the
/// HTLC VTXOs. The exit publishes the HTLC preimage on-chain, which the
/// server's HtlcSettler extracts to settle the hold invoice.

include!("lightning_fallback.rs");

include!("lightning_helpers.rs");
