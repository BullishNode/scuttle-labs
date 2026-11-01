use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::bail;
use bitcoin::{Address, address::NetworkUnchecked};


use bitcoin_ext::{BlockDelta, BlockHeight, BlockRef};
use bitcoin_ext::rpc::{BitcoinRpcExt, BitcoinRpcClient, RpcApi};

use server::sync::{ChainEventListener, RawMempool, BlockData};
use server::sync::block_index::{BlockIndex};
use server::database::{BlockTable, Db, Tx};

use ark_testing::TestContext;

/// Checks that the block index is fully consistent with the bitcoind chain
async fn check_block_index_is_consistent(db: &Db, block_table: BlockTable, bitcoind: &BitcoinRpcClient) {

	let lowest_block = db.read(async |t| t.get_lowest_block(block_table).await).await.unwrap().unwrap();
	let highest_block = db.read(async |t| t.get_highest_block(block_table).await).await.unwrap().unwrap();

	for height in lowest_block.height.to_u32()..=highest_block.height.to_u32() {
		let bitcoind_block = bitcoind.get_block_by_height(BlockHeight::new(height)).unwrap();
		let db_block = db.read(async |t| t.get_block_by_height(block_table, BlockHeight::new(height)).await).await.unwrap().unwrap();

		assert_eq!(bitcoind_block.hash, db_block.hash);
	}
}

pub struct BlockIndexListener {
	tip: parking_lot::Mutex<Option<BlockRef>>,
}

impl BlockIndexListener {
	pub fn new() -> Self {
		Self { tip: parking_lot::Mutex::new(None) }
	}
}

#[async_trait::async_trait]
impl ChainEventListener for BlockIndexListener {

	async fn on_mempool_update(&self, _raw: &RawMempool) -> anyhow::Result<()> {
		panic!("This should never be called")
	}


	async fn on_block_added(&self, block: &BlockData) -> anyhow::Result<()> {
		let mut tip = self.tip.lock();
		match *tip {
			None => {
				*tip = Some(block.block_ref);
			},
			Some(prev) => {
				if block.block_ref.height == prev.height + BlockDelta::new(1) {
					*tip = Some(block.block_ref);
				} else if block.block_ref.height == prev.height && block.block_ref.hash == prev.hash {
					// Already at this tip, do nothing
				} else {
					panic!(
						"BlockIndexListener: unexpected block addition: prev tip = {:?}, new block = {:?}",
						prev, block.block_ref
					);
				}
			}
		}

		Ok(())
	}

	async fn on_reorg(&self, _tx: &Tx<'_>, block: BlockRef) -> anyhow::Result<()> {
		if let Some(tip) = *self.tip.lock() {
			if block.height > tip.height {
				panic!(
					"BlockIndexListener: reorg to block at height {} which is higher than current tip {}",
					block.height, tip.height
				);
			}
		}


		let mut tip = self.tip.lock();
		*tip = Some(block);

		Ok(())
	}
}

#[tokio::test]
async fn test_block_index_basic_sync() {
	// Setup context and start bitcoind and postgres as in the minimal setup
	let mut ctx = TestContext::new_minimal("block_index/basic").await;

	// Initializes the postgres database and connects to it
	ctx.init_central_postgres().await;
	let postgres_cfg = ctx.new_postgres(&ctx.test_name).await;
	Db::create(&postgres_cfg, bitcoin::Network::Regtest).await.expect("Database created");
	let db = Db::connect(&postgres_cfg, bitcoin::Network::Regtest).await.expect("Connected to database");

	// Initializes a bitcoind and creates a client
	ctx.init_central_bitcoind().await;
	let bitcoind = ctx.bitcoind().sync_client();
	let bitcoind_async = ctx.bitcoind().async_client();

	// Create the BlockIndex (should sync tip with deep_tip from bitcoind)
	let listeners = vec![Box::new(BlockIndexListener::new()) as Box<dyn ChainEventListener>];
	let birthday = bitcoind.deep_tip().expect("deep tip");
	let mut block_index = BlockIndex::new(bitcoind_async, db.clone(), listeners, birthday, BlockTable::Captaind).await.expect("blockindex created");
	check_block_index_is_consistent(&db, BlockTable::Captaind, &bitcoind).await;

	// The current chain-tip is 104
	// However, we are only synced up-to the deep tip
	let chain_tip = block_index.chain_tip();
	let sync_height = block_index.sync_tip();
	assert_eq!(chain_tip.height, BlockHeight::new(104));
	assert_eq!(sync_height.height, BlockHeight::new(4));

	// Generate 20 blocks and validate that the index processes them correctly
	let addrs = Address::<NetworkUnchecked>::from_str("bcrt1p28cpcjynxvz3pyvd99wu7f5uxxkflttec6t4sxndxdsgtxksnp7q90rfcv").unwrap().assume_checked();
	bitcoind.generate_to_address(20, &addrs).expect("Generated 20 blocks");
	block_index.sync().await.expect("BlockIndex sync");
	check_block_index_is_consistent(&db, BlockTable::Captaind, &bitcoind).await;

	// The updated tip goes to block-height 124
	{
		let chain_tip = block_index.chain_tip();
		let sync_tip = block_index.sync_tip();

		let bitcoind_tip = bitcoind.tip().expect("Got bitcoind tip");
		assert_eq!(chain_tip.height, bitcoind_tip.height);
		assert_eq!(chain_tip.hash, bitcoind_tip.hash);
		assert_eq!(chain_tip.height, BlockHeight::new(124));

		// After sync, sync_tip should equal chain_tip
		assert_eq!(sync_tip.height, chain_tip.height);
		assert_eq!(sync_tip.hash, chain_tip.hash);
	}

	// Let's retrieve the details about the block at heigh 120
	let block_120 = {
		let local_block = db.read(async |t| t.get_block_by_height(BlockTable::Captaind, BlockHeight::new(120)).await).await.unwrap().unwrap();
		let bitcoin_block = bitcoind.get_block_by_height(BlockHeight::new(120)).unwrap();
		assert_eq!(local_block.hash, bitcoin_block.hash);
		assert_eq!(local_block.height, BlockHeight::new(120));
		assert_eq!(bitcoin_block.height, BlockHeight::new(120));
		local_block
	};

	// Let's invalidate this block and introduce a re-org
	// use a enw address so that bitcoind acceptst the new block
	let addrs = Address::<NetworkUnchecked>::from_str("bcrt1pnvttf55269k90h8r4xcwewqr9nvlyngge06srk4gmddu6sjjk9gq82vrkf").unwrap().assume_checked();
	bitcoind.invalidate_block(&block_120.hash).expect("Invalidated block");
	// Core can select a canonical tip below our current height. Roll back
	// against that tip before replacements arrive, without panicking.
	block_index.sync().await.expect("shorter canonical tip sync");
	assert_eq!(block_index.sync_tip().height, BlockHeight::new(119));
	check_block_index_is_consistent(&db, BlockTable::Captaind, &bitcoind).await;
	bitcoind.generate_to_address(20, &addrs).expect("Generated 1 block");

	// Let's verify that our block-index manages this correctly
	block_index.sync().await.expect("BlockIndex sync");
	check_block_index_is_consistent(&db, BlockTable::Captaind, &bitcoind).await;
	{
		let bitcoind_tip = bitcoind.tip().expect("Got bitcoind tip");
		let chain_tip = block_index.chain_tip();
		let sync_tip = block_index.sync_tip();
		let db_tip = db.read(async |t| t.get_highest_block(BlockTable::Captaind).await).await.unwrap().unwrap();

		assert_eq!(chain_tip.height, bitcoind_tip.height);
		assert_eq!(chain_tip.hash, bitcoind_tip.hash);
		assert_eq!(bitcoind_tip.height, BlockHeight::new(139));
		assert_eq!(chain_tip.height, BlockHeight::new(139));
		assert_eq!(db_tip.height, BlockHeight::new(139));

		// After sync, sync_tip should equal chain_tip
		assert_eq!(sync_tip.height, chain_tip.height);
		assert_eq!(sync_tip.hash, chain_tip.hash);
	}
}

/// A listener that fails the first `on_reorg` call and succeeds afterwards.
pub struct FailOnceReorgListener {
	calls: Arc<AtomicUsize>,
	completed: Arc<AtomicUsize>,
	cache_calls: Arc<AtomicUsize>,
	db: Db,
}

impl FailOnceReorgListener {
	pub fn new(db: Db) -> Self {
		Self { calls: Arc::new(AtomicUsize::new(0)), completed: Arc::new(AtomicUsize::new(0)),
			cache_calls: Arc::new(AtomicUsize::new(0)), db }
	}
}

#[async_trait::async_trait]
impl ChainEventListener for FailOnceReorgListener {
	async fn on_mempool_update(&self, _raw: &RawMempool) -> anyhow::Result<()> {
		panic!("This should never be called")
	}

	async fn on_block_added(&self, _block: &BlockData) -> anyhow::Result<()> {
		Ok(())
	}

	async fn on_sync_complete(&self, point: BlockRef) -> anyhow::Result<()> {
		let published = self.db.read(async |t| t.get_highest_block(BlockTable::Captaind).await).await?;
		assert_eq!(published, Some(point), "completion precedes database tip publication");
		self.completed.fetch_add(1, Ordering::SeqCst);
		Ok(())
	}

	async fn on_reorg(&self, tx: &Tx<'_>, _block: BlockRef) -> anyhow::Result<()> {
		assert!(tx.query_one("SELECT invalid FROM expiry_chain_view WHERE id=1", &[]).await?.get::<_, bool>(0),
			"rollback callbacks must share the invalidated outer transaction");
		let calls = self.calls.fetch_add(1, Ordering::SeqCst);
		if calls == 0 {
			bail!("simulated reorg failure");
		}
		Ok(())
	}

	async fn on_reorg_committed(&self, block: BlockRef) -> anyhow::Result<()> {
		let tip = self.db.read(async |t| t.get_highest_block(BlockTable::Captaind).await).await?;
		assert_eq!(tip, Some(block), "cache refresh must observe the committed rollback with one pooled connection");
		if self.cache_calls.fetch_add(1, Ordering::SeqCst) == 0 {
			bail!("simulated postcommit cache failure");
		}
		Ok(())
	}
}

/// Regression test for the reorg ordering bug in `BlockIndex::org_out_blocks_above`.
///
/// If a listener fails while handling `on_reorg`, the in-memory sync height must
/// not be advanced until the listeners succeed and the orphaned rows are deleted.
/// Otherwise the next sync cycle no longer detects the reorg and wedges on the
/// duplicate block inserts.
#[tokio::test]
async fn test_block_index_reorg_retry() {
	let mut ctx = TestContext::new_minimal("block_index/reorg_retry").await;

	ctx.init_central_postgres().await;
	let mut postgres_cfg = ctx.new_postgres(&ctx.test_name).await;
	postgres_cfg.max_connections = 1;
	Db::create(&postgres_cfg, bitcoin::Network::Regtest).await.expect("Database created");
	let db = Db::connect(&postgres_cfg, bitcoin::Network::Regtest).await.expect("Connected to database");

	ctx.init_central_bitcoind().await;
	let bitcoind = ctx.bitcoind().sync_client();
	let bitcoind_async = ctx.bitcoind().async_client();

	let listener = FailOnceReorgListener::new(db.clone());
	let completed = listener.completed.clone();
	let call_count = listener.calls.clone();
	let cache_calls = listener.cache_calls.clone();
	let listeners = vec![Box::new(listener) as Box<dyn ChainEventListener>];
	let birthday = bitcoind.deep_tip().expect("deep tip");
	let mut block_index = BlockIndex::new(bitcoind_async, db.clone(), listeners, birthday, BlockTable::Captaind).await.expect("blockindex created");

	// Sync up to the chain tip.
	let addrs = Address::<NetworkUnchecked>::from_str("bcrt1p28cpcjynxvz3pyvd99wu7f5uxxkflttec6t4sxndxdsgtxksnp7q90rfcv").unwrap().assume_checked();
	bitcoind.generate_to_address(20, &addrs).expect("Generated 20 blocks");
	block_index.sync().await.expect("BlockIndex sync");

	// Invalidate a block and mine replacements, causing a reorg.
	let block_120 = db.read(async |t| t.get_block_by_height(BlockTable::Captaind, BlockHeight::new(120)).await).await.unwrap().unwrap();
	let addrs2 = Address::<NetworkUnchecked>::from_str("bcrt1pnvttf55269k90h8r4xcwewqr9nvlyngge06srk4gmddu6sjjk9gq82vrkf").unwrap().assume_checked();
	bitcoind.invalidate_block(&block_120.hash).expect("Invalidated block");
	bitcoind.generate_to_address(20, &addrs2).expect("Generated replacement blocks");

	// First sync should fail because the listener fails once.
	let result = block_index.sync().await;
	assert!(result.is_err(), "First sync should fail due to listener failure");
	assert_eq!(completed.load(Ordering::SeqCst), 1, "failed listener must not acknowledge a completed sync");

	// The retry commits SQL, but a failed cache refresh must neither publish
	// completion nor make the following ancestor lookup use deleted blocks.
	assert!(block_index.sync().await.is_err());
	assert_eq!(completed.load(Ordering::SeqCst), 1);
	block_index.sync().await.expect("Third sync should retry caches and converge");

	check_block_index_is_consistent(&db, BlockTable::Captaind, &bitcoind).await;

	// The listener must have been called twice: one failure, then one success.
	assert_eq!(call_count.load(Ordering::SeqCst), 2);
	assert_eq!(cache_calls.load(Ordering::SeqCst), 2, "cache retry must not repeat the committed rollback");
	assert_eq!(completed.load(Ordering::SeqCst), 2);
	assert!(!block_index.sync().await.unwrap());
	assert_eq!(completed.load(Ordering::SeqCst), 3, "equal-tip restart cycles must acknowledge completion");
}

/// This listener changes real Core-backed block observations through the
/// transaction supplied by BlockIndex, never a second pooled connection.
struct RollbackObservationListener {
	entered: tokio::sync::mpsc::UnboundedSender<(i32, chrono::DateTime<chrono::Utc>)>,
	release: Option<Arc<tokio::sync::Notify>>,
	calls: Arc<AtomicUsize>,
	mutations: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChainEventListener for RollbackObservationListener {
	async fn on_mempool_update(&self, _raw: &RawMempool) -> anyhow::Result<()> {
		panic!("BlockIndex does not poll the mempool")
	}

	async fn on_block_added(&self, _block: &BlockData) -> anyhow::Result<()> { Ok(()) }

	async fn on_reorg(&self, tx: &Tx<'_>, fork: BlockRef) -> anyhow::Result<()> {
		assert!(tx.query_one("SELECT invalid FROM expiry_chain_view WHERE id=1", &[]).await?.get::<_, bool>(0),
			"the actual rollback callback must enter with an invalid view");
		let backend = tx.query_one("SELECT pid,backend_start FROM pg_stat_activity WHERE pid=pg_backend_pid()", &[]).await?;
		self.calls.fetch_add(1, Ordering::SeqCst);
		self.entered.send((backend.get(0), backend.get(1))).unwrap();
		if let Some(release) = &self.release { release.notified().await; }
		// These rows were populated by a second real BlockIndex, not synthetic
		// observation or economic rows. Their rollback must share this barrier.
		tx.remove_blocks_above(BlockTable::Watchmand, fork.height).await?;
		self.mutations.fetch_add(1, Ordering::SeqCst);
		Ok(())
	}
}

#[tokio::test]
async fn block_index_publication_lock_defers_real_rollback() {
	block_index_rollback_publication_order(true).await;
}

#[tokio::test]
async fn block_index_invalidation_refuses_stale_publication() {
	block_index_rollback_publication_order(false).await;
}

async fn block_index_rollback_publication_order(publication_wins: bool) {
	let mut ctx = TestContext::new_minimal(if publication_wins { "block_index/publication_wins" }
		else { "block_index/invalidation_wins" }).await;
	ctx.init_central_postgres().await;
	let mut postgres_cfg = ctx.new_postgres(&ctx.test_name).await;
	// One holder, one blocked contender, one independent observer. The
	// separately retained pool1/cache-retry case has a different purpose.
	postgres_cfg.max_connections = 3;
	Db::create(&postgres_cfg, bitcoin::Network::Regtest).await.unwrap();
	let db = Db::connect(&postgres_cfg, bitcoin::Network::Regtest).await.unwrap();
	ctx.init_central_bitcoind().await;
	let core = ctx.bitcoind().sync_client();
	let birthday = core.deep_tip().unwrap();
	let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
	let release_callback = Arc::new(tokio::sync::Notify::new());
	let calls = Arc::new(AtomicUsize::new(0));
	let mutations = Arc::new(AtomicUsize::new(0));
	let listener = RollbackObservationListener { entered: entered_tx,
		release: (!publication_wins).then(|| release_callback.clone()), calls: calls.clone(), mutations: mutations.clone() };
	let mut index = BlockIndex::new(ctx.bitcoind().async_client(), db.clone(),
		vec![Box::new(listener)], birthday, BlockTable::Captaind).await.unwrap();
	let mut watch_index = BlockIndex::new(ctx.bitcoind().async_client(), db.clone(),
		Vec::new(), birthday, BlockTable::Watchmand).await.unwrap();
	index.sync().await.unwrap();
	watch_index.sync().await.unwrap();
	let view = db.read(async |tx| Ok(tx.expiry_view().await?.unwrap())).await.unwrap();
	assert_eq!(view.processed, core.tip().unwrap());
	let old_tip = view.processed;
	let old_block = core.get_block(&old_tip.hash).unwrap();
	// An actual Core transaction is sufficient for a pre-publication gate
	// probe. It is not a valid expiry parent and is never published.
	let gate_probe = old_block.txdata[0].clone();
	let release_publication = Arc::new(tokio::sync::Notify::new());
	let publication = if publication_wins {
		let (held_tx, held_rx) = tokio::sync::oneshot::channel();
		let publication_db = db.clone();
		let release = release_publication.clone();
		let task = tokio::spawn(async move {
			publication_db.write(async |tx| {
				tx.lock_valid_expiry_view(view).await?;
				held_tx.send(tx.query_one("SELECT pg_backend_pid()", &[]).await?.get::<_, i32>(0)).unwrap();
				release.notified().await;
				tx.lock_valid_expiry_view(view).await
			}).await
		});
		Some((held_rx.await.unwrap(), task))
	} else { None };
	core.invalidate_block(&old_tip.hash).unwrap();
	let fork = core.tip().unwrap();
	assert!(fork.height < old_tip.height);
	let rollback = tokio::spawn(async move {
		index.sync().await?;
		Ok::<_, anyhow::Error>(index)
	});
	if let Some((holder, publication)) = publication {
		// Observe the REAL BlockIndex invalidation waiting, not merely a task
		// that has been spawned but has not attempted its conflicting lock.
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let blocked = db.read(async |tx| {
					tx.query_one("SELECT pg_stat_clear_snapshot()", &[]).await?;
					Ok(tx.query_one("SELECT COUNT(*) FROM pg_stat_activity a WHERE a.datname=current_database() \
						AND a.query='SELECT generation FROM expiry_chain_view WHERE id=1 FOR UPDATE' \
						AND $1=ANY(pg_blocking_pids(a.pid))", &[&holder]).await?.get::<_, i64>(0))
				}).await.unwrap();
				if blocked == 1 { break; }
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		}).await.expect("actual BlockIndex invalidation must wait behind publication");
		assert!(!rollback.is_finished());
		assert_eq!(calls.load(Ordering::SeqCst), 0);
		assert_eq!(mutations.load(Ordering::SeqCst), 0);
		assert_eq!(db.read(async |tx| tx.get_highest_block(BlockTable::Watchmand).await).await.unwrap(), Some(old_tip));
		assert_eq!(db.read(async |tx| tx.expiry_view().await).await.unwrap(), Some(view));
		release_publication.notify_one();
		publication.await.unwrap().unwrap();
	} else {
		let holder = tokio::time::timeout(Duration::from_secs(10), entered_rx.recv()).await.unwrap().unwrap();
		assert_eq!(calls.load(Ordering::SeqCst), 1);
		assert_eq!(mutations.load(Ordering::SeqCst), 0);
		// Invalidation must already be durable outside the callback's open
		// transaction; a crash cannot resurrect the old completed generation.
		assert!(db.read(async |tx| tx.expiry_view().await).await.unwrap().is_none());
		let (attempt_tx, attempt_rx) = tokio::sync::oneshot::channel();
		let publication_db = db.clone();
		let stale = tokio::spawn(async move {
			publication_db.write(async |tx| {
				let row = tx.query_one("SELECT pid,backend_start FROM pg_stat_activity WHERE pid=pg_backend_pid()", &[]).await?;
				attempt_tx.send((row.get::<_, i32>(0), row.get::<_, chrono::DateTime<chrono::Utc>>(1))).unwrap();
				// This production consumer checks the view BEFORE any allocation
				// lookup. No economic row is fabricated for this early gate probe.
				tx.store_expiry_parent(view, i64::MAX, &gate_probe).await
			}).await
		});
		let contender = attempt_rx.await.unwrap();
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let blocked = db.read(async |tx| {
					tx.query_one("SELECT pg_stat_clear_snapshot()", &[]).await?;
					Ok(tx.query_one("SELECT COUNT(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid \
						WHERE a.pid=$1 AND a.backend_start=$2 AND NOT l.granted AND $3=ANY(pg_blocking_pids(a.pid))",
						&[&contender.0, &contender.1, &holder.0]).await?.get::<_, i64>(0))
				}).await.unwrap();
				if blocked == 1 { break; }
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		}).await.expect("old-view parent gate must wait behind the actual rollback transaction");
		assert!(!stale.is_finished());
		assert_eq!(db.read(async |tx| tx.get_highest_block(BlockTable::Watchmand).await).await.unwrap(), Some(old_tip));
		release_callback.notify_one();
		let error = stale.await.unwrap().unwrap_err();
		assert!(format!("{error:#}").contains("expiry view is invalidated or differs from the completed view"),
			"must refuse the old view, not merely fail on the intentionally absent allocation: {error:#}");
	}
	let index = rollback.await.unwrap().unwrap();
	assert_eq!(index.sync_tip(), fork);
	assert_eq!(calls.load(Ordering::SeqCst), 1);
	assert_eq!(mutations.load(Ordering::SeqCst), 1);
	assert_eq!(db.read(async |tx| tx.get_highest_block(BlockTable::Watchmand).await).await.unwrap(), Some(fork));
	assert_eq!(db.read(async |tx| tx.get_highest_block(BlockTable::Captaind).await).await.unwrap(), Some(fork));
	assert!(db.read(async |tx| tx.expiry_view().await).await.unwrap().is_none());
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT COUNT(*) FROM expiry_allocation", &[])
		.await?.get::<_, i64>(0))).await.unwrap(), 0, "the early stale-view probe must create no economic row");
	watch_index.sync().await.unwrap();
	assert_eq!(db.read(async |tx| Ok(tx.expiry_view().await?.unwrap().processed)).await.unwrap(), fork);
	check_block_index_is_consistent(&db, BlockTable::Captaind, &core).await;
	check_block_index_is_consistent(&db, BlockTable::Watchmand, &core).await;
}

