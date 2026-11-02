use ark_testing::util::ToAltString;
use std::collections::BTreeSet;

use bitcoin::{Amount, Network, OutPoint, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use bitcoin::secp256k1::{rand, Keypair};
use chrono::{DateTime, Local};
use bitcoin::hashes::Hash;

use ark::{ServerVtxo, ServerVtxoPolicy, VtxoId, VtxoPolicy};
use ark::arkoor::{ArkoorDestination, package::ArkoorPackageBuilder};
use ark::board::BoardBuilder;
use ark::vtxo::Full;
use bitcoin_ext::{BlockDelta, BlockHeight, BlockRef, TxOutExt};
use ark::encode::ProtocolEncoding;
use ark::fallback::OwnerLink;
use ark::mailbox::MailboxIdentifier;
use ark::test_util::VTXO_VECTORS;

use server::OwnershipRefusal;
use server::owner_proof::OwnerLinks;
use server::database::{BlockTable, Db, SpendState};
use server::database::expiry_chain::ExpiryViewToken;
use server::database::expiry_decision::{ReadyCoin, ReadyOutput, VerifiedPayee};
use server::database::expiry_inventory::{BackingPartitionResidual, BackingSnapshotLimits};
use server::database::expiry_attribution::Attribution;
use server::database::expiry_ownership::{OwnershipDisposition, OwnershipEvidence, OwnershipProposal};
use server::database::tree::VtxoTreeUpdate;

use ark_testing::TestContext;

/// Helper: create a db for a test.
async fn test_db(name: &str) -> (TestContext, Db) {
	let mut ctx = TestContext::new_minimal(&format!("postgresd/tree/{}", name)).await;
	ctx.init_central_postgres().await;
	let postgres_cfg = ctx.new_postgres(&ctx.test_name).await;
	Db::create(&postgres_cfg).await.expect("Database created");
	let db = Db::connect(&postgres_cfg).await.expect("Connected to database");
	super::super::register_vector_owners(&db).await;
	(ctx, db)
}

/// Helper: dummy txid (different from any vtxo-related txid).
fn dummy_txid(byte: u8) -> Txid {
	Txid::from_byte_array([byte; 32])
}

/// Helper: number of history records and `updated_at` of a vtxo.
async fn vtxo_history_state(db: &Db, vtxo_id: VtxoId) -> (i64, DateTime<Local>) {
	db.read(async |t| {
		let row = t.query_one(
			"SELECT
				(SELECT COUNT(*) FROM vtxo_history WHERE vtxo_id = $1),
				(SELECT updated_at FROM vtxo WHERE vtxo_id = $1)",
			&[&vtxo_id.to_string()],
		).await?;
		Ok((row.get(0), row.get(1)))
	}).await.expect("query vtxo history")
}

include!("tree_fallback.rs");
