mod tree;

use std::str::FromStr;
use std::time::Duration;

use bitcoin::secp256k1::{Keypair, PublicKey, Secp256k1, SecretKey};
use chrono::{Local, TimeDelta};

use ark::{ServerVtxo, VtxoId, VtxoPolicy};
use ark::lightning::{Invoice, Preimage};
use ark::mailbox::{MailboxIdentifier, MailboxType};
use ark::test_util::VTXO_VECTORS;
use ark::tree::signed::UnlockHash;

use bitcoin::hashes::Hash;
use bark::lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};
use bitcoin_ext::{BlockDelta, BlockHeight};

use server::database::Db;
use server::database::htlc_vtxo::{self, HtlcDirection};
use server::database::tree::VtxoTreeUpdate;
use server::database::ln::{HtlcSettlementOrigin, LightningHtlcSubscriptionStatus, SourceSettlement};
use server::database::xpay::{self, ClnXpayStatus, NewClnXpay};
use server::ln::LightningFailureKind;

use ark_testing::{sat, TestContext};

const DUMMY_PUBKEY: &str = "038f47dcd43ba6d97fc9ed2e3bba09b175a45fac55f0683e8cf771e8ced4572354";
const BOLT11_INVOICE: &str = "lnbcrt11p59rr6msp534kz2tahyrxl0rndcjrt8qpqvd0dynxxwfd28ea74rxjuj0tphfspp5nc0gf6vamuphaf4j49qzjvz2rg3del5907vdhncn686cj5yykvfsdqqcqzzs9qyysgqgalnpu3selnlgw8n66qmdpuqdjpqak900ru52v572742wk4mags8a8nec2unls57r5j95kkxxp4lr6wy9048uzgsvdhrz7dh498va2cq4t6qh8";

include!("mod_fallback.rs");
