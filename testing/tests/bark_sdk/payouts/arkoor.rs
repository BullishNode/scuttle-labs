use std::time::Duration;
use tokio::sync::Notify;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use ark::{ProtocolEncoding, VtxoPolicy};
use ark_testing::daemon::captaind::{self, ArkClient};
use server_rpc::protos;
use ark::address::VtxoDelivery;
use bark::movement::MovementStatus;

use ark_testing::{TestContext, sat};
use ark_testing::balance::assert_balance_consistent;

include!("arkoor_fallback.rs");
