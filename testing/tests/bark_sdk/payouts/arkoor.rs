use std::time::Duration;
use tokio::sync::Notify;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use ark::{ProtocolEncoding, VtxoPolicy};
use ark::attestations::VtxoStatusAttestation;
use ark::mailbox::BlindedMailboxIdentifier;
use ark_testing::daemon::captaind::{self, ArkClient};
use bitcoin::secp256k1::PublicKey;
use server_rpc::protos;
use ark::address::VtxoDelivery;
use bark::movement::MovementStatus;

use ark_testing::{TestContext, sat};
use ark_testing::balance::assert_balance_consistent;
use server::database::Db;

include!("arkoor_fallback.rs");
