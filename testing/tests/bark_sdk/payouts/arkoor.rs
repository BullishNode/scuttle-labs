

use ark::ProtocolEncoding;
use ark::attestations::VtxoStatusAttestation;
use ark::mailbox::BlindedMailboxIdentifier;
use bitcoin::secp256k1::PublicKey;
use server_rpc::protos;
use ark::address::VtxoDelivery;

use ark_testing::{TestContext, sat};
use server::database::Db;

include!("arkoor_fallback.rs");
