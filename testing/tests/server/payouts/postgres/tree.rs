

use ark::ServerVtxo;
use ark::test_util::VTXO_VECTORS;

use server::database::{Db, SpendState};
use server::database::tree::VtxoTreeUpdate;

use ark_testing::TestContext;

/// Helper: create a db for a test.
async fn test_db(name: &str) -> (TestContext, Db) {
	let mut ctx = TestContext::new_minimal(&format!("postgresd/tree/{}", name)).await;
	ctx.init_central_postgres().await;
	let postgres_cfg = ctx.new_postgres(&ctx.test_name).await;
	Db::create_with_payouts(&postgres_cfg, true).await.expect("Database created");
	let db = Db::connect(&postgres_cfg).await.expect("Connected to database");
	(ctx, db)
}

include!("tree_fallback.rs");
