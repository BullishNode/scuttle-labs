use std::time::Duration;

use bitcoin::secp256k1::{Keypair, rand::thread_rng};

use ark::{ProtocolEncoding, ServerVtxo, SECP};
use ark::mailbox::{MailboxAuthorization, MailboxIdentifier, MailboxType};
use ark::test_util::dummy::DummyTestVtxoSpec;

use server::database::Db;
use server_rpc::protos;
use server_rpc::protos::mailbox_server::mailbox_message::Message;

use ark_testing::TestContext;

include!("mailbox_fallback.rs");
