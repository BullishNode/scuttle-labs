mod tree;

use std::str::FromStr;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};

use ark::{ServerVtxo, VtxoId};
use ark::lightning::Invoice;
use ark::test_util::VTXO_VECTORS;

use bark::lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};

use server::database::Db;
use server::database::htlc_vtxo::{self, HtlcDirection};
use server::database::tree::VtxoTreeUpdate;

use ark_testing::TestContext;

const DUMMY_PUBKEY: &str = "038f47dcd43ba6d97fc9ed2e3bba09b175a45fac55f0683e8cf771e8ced4572354";

include!("mod_fallback.rs");
