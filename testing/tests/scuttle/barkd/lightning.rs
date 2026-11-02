
use std::str::FromStr;
use std::time::Duration;

use bark_json::cli::LightningReceiveStateKind;

use ark_testing::{btc, sat, TestContext};
use ark_testing::constants::BOARD_CONFIRMATIONS;
use ark_testing::util::poll_interval;
use ark::{ProtocolEncoding, Vtxo};
use ark::lightning::Invoice;
use ark::mailbox::MailboxIdentifier;
use ark::vtxo::Full;
use bark_rest_client::apis::lightning_api;
use bark_rest_client::models::LightningInvoiceForAddressRequest;
use bitcoin::Amount;
use server::database::Db;
use server::database::expiry_attribution::Attribution;


include!("lightning_fallback.rs");
