use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;

use ark::lightning::PaymentHash;
use ark_testing::{Captaind, TestContext, btc, sat, util::FutureExt};
use ark_testing::balance::assert_balance_consistent;
use ark_testing::context::LightningPaymentSetup;

use bark::actions::lightning::receive::{LightningReceiveState, Progress};
use bark::movement::{MovementId, MovementStatus};
use bark::subsystem::Subsystem;
use bark::{OpenWalletArgs, Wallet, WalletSeed};
use bark::onchain::{OnchainWallet, OnchainWalletTrait};
use bark::persist::sqlite::SqliteClient;
use bitcoin::Network;
use cln_rpc::plugins::hold;
use server_rpc::protos;
use server_rpc::protos::mailbox_server::mailbox_message::Message as MailboxMsg;
use ark_testing::daemon::captaind::{self, ArkClient};


include!("lightning_fallback.rs");

include!("lightning_helpers.rs");
