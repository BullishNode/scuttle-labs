use std::sync::Arc;
use std::time::Duration;

use bitcoin::{Amount, OutPoint, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin::secp256k1::Keypair;

use ark::{ProtocolEncoding, Vtxo, VtxoPolicy, SECP};
use ark::attestations::VtxoStatusAttestation;
use ark::mailbox::MailboxIdentifier;
use bitcoin_ext::{BlockDelta, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use ark::vtxo::Full;
use bark::Wallet;
use server_rpc::protos;
use server::database::Db;
use server::vtxopool::VtxoTarget;

use ark_testing::{Captaind, TestContext, btc, sat};
use ark_testing::util::{poll_interval, FutureExt};

use cln_rpc::plugins::hold;

/// Verify that the server extracts preimages from on-chain HTLC spends
/// and uses them to settle invoices.
///
/// The proxy blocks cooperative settlement, so bark explicitly exits the
/// HTLC VTXOs. The exit publishes the HTLC preimage on-chain, which the
/// server's HtlcSettler extracts to settle the hold invoice.

include!("lightning_fallback.rs");

include!("lightning_helpers.rs");
