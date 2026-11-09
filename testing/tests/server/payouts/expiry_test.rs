//! Shared canonical-chain assertions for expiry payout scenarios.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::time::Duration;

use ark::{ProtocolEncoding, Vtxo, VtxoId};
use ark::mailbox::MailboxIdentifier;
use ark::vtxo::{Full, TapScriptClause};
use ark_testing::{sat, Captaind, TestContext};
use ark_testing::daemon::captaind::WalletAdminClient;
use ark_testing::daemon::watchmand::Watchmand;
use bark::Wallet;
use bark::vtxo::VtxoState;
use bitcoin::{Amount, BlockHash, OutPoint, ScriptBuf, Transaction, Txid, TxOut};
use bitcoin::consensus::deserialize;
use bitcoin::hex::DisplayHex;
use bitcoin_ext::{BlockHeight, BlockRef, TxOutExt};
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use server::database::Db;
use server_rpc::protos;

pub(super) async fn rebuilt(ctx: &TestContext, srv: &Captaind, db: &Db) {
	let height = bitcoin_ext::BlockHeight::new(u32::try_from(ctx.bitcoind().get_block_count().await).unwrap());
	srv.wait_for_sync_height(height).await;
	srv.watchmand().wait_for_sync_height(height).await;
	let point = BlockRef { height, hash: ctx.bitcoind().sync_client().get_best_block_hash().unwrap() };
	wait_rebuilt(db, point, Duration::from_secs(15), "watchmand's completed expiry view must bind the real Core tip").await;
}

pub(super) async fn actual_fee(ctx: &TestContext, transaction: &Transaction) -> Amount {
	let mut total = Amount::ZERO;
	let mut seen = BTreeSet::new();
	for input in &transaction.input {
		assert!(seen.insert(input.previous_output));
		let previous = ctx.bitcoind().await_transaction(input.previous_output.txid).await;
		total += previous.output[input.previous_output.vout as usize].value;
	}
	total.checked_sub(transaction.output.iter().map(|output| output.value).sum::<Amount>()).unwrap()
}

pub(super) struct Epoch {
	pub(super) parents: BTreeMap<Txid, Transaction>,
	pub(super) children: BTreeMap<Txid, Transaction>,
	pub(super) outputs: BTreeMap<VtxoId, Option<OutPoint>>,
	pub(super) hashes: BTreeMap<Txid, BlockHash>,
}

pub(super) async fn watchman_coins(watchman: &Watchmand) -> BTreeSet<OutPoint> {
	let address = watchman.config().admin_address.unwrap();
	let mut rpc = WalletAdminClient::connect(format!("http://{address}")).await.unwrap();
	rpc.wallet_status(protos::Empty {}).await.unwrap().into_inner().watchman.unwrap()
		.confirmed_utxos.into_iter().map(|point| OutPoint::from_str(&point).unwrap()).collect()
}

pub(super) fn canonical(ctx: &TestContext, tx: &Transaction) -> BlockRef {
	let core = ctx.bitcoind().sync_client();
	let info = core.custom_get_raw_transaction_info(tx.compute_txid(), None).unwrap().unwrap();
	assert!(info.confirmations.unwrap_or(0) >= 6);
	let hash = info.blockhash.unwrap();
	let header = core.get_block_header_info(&hash).unwrap();
	assert_eq!(core.get_block_hash(header.height as u64).unwrap(), hash);
	assert!(core.get_block(&hash).unwrap().txdata.iter().any(|actual| actual == tx));
	BlockRef { height: BlockHeight::new(u32::try_from(header.height).unwrap()), hash }
}

pub(crate) async fn wait_rebuilt(db: &Db, point: BlockRef, timeout: Duration, message: &str) -> BlockRef {
	tokio::time::timeout(timeout, async {
		loop {
			let generation = db.read(async |tx| {
				let Some(view) = tx.expiry_view().await? else { return Ok(None); };
				if view == point && (tx.expiry_view().await? == Some(view)) {
					Ok(Some(view))
				} else { Ok(None) }
			}).await.unwrap();
			if let Some(generation) = generation { return generation; }
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect(message)
}

#[derive(Clone)]
pub(super) struct ExpectedLeaf {
	pub(super) coin: Vtxo<Full>,
	pub(super) mailbox: MailboxIdentifier,
	pub(super) script: ScriptBuf,
	pub(super) fee: u64,
	pub(super) payout: u64,
	pub(super) forfeited: u64,
	pub(super) group_payout: u64,
}

pub(super) async fn confirm_epoch(
	ctx: &TestContext, srv: &Captaind, db: &Db, leaves: &[ExpectedLeaf], start: u64, progress_budget: u64, final_budget: u64,
) -> Epoch {
	let expiry = leaves[0].coin.expiry_height().to_u32();
	assert!(leaves.iter().all(|leaf| leaf.coin.expiry_height().to_u32() == expiry));
	let height = u32::try_from(ctx.bitcoind().get_block_count().await).unwrap();
	assert!(height < expiry);
	ctx.generate_blocks(expiry + 1 - height).await;
	rebuilt(ctx, srv, db).await;
	let ids = leaves.iter().map(|leaf| leaf.coin.id().to_string()).collect::<Vec<_>>();
	let core = ctx.bitcoind().sync_client();
	let rows = tokio::time::timeout(Duration::from_secs(40), async {
		for _ in 0..20 {
			let rows = db.read(async |tx| Ok(tx.query("SELECT e.vtxo_id,c.gross_sat,c.fee_sat,c.payout_sat, \
				c.forfeited_sat,c.spk,c.owner_mailbox,c.user_pubkey,a.id,a.sweep_tx,a.sweep_txid \
				FROM expiry_entitlement e JOIN expiry_allocation_coin c ON c.entitlement_id=e.id \
				JOIN expiry_allocation_state a ON a.id=c.allocation_id WHERE e.vtxo_id=ANY($1) \
				AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='customer' AND a.broadcast_at IS NOT NULL", &[&ids],
			).await?)).await.unwrap();
			assert!(rows.len() <= leaves.len());
			if rows.len() == leaves.len() && rows.iter().all(|row|
				core.custom_get_raw_transaction_info(row.get::<_, &str>("sweep_txid").parse().unwrap(), None).unwrap().is_some()) {
				return rows;
			}
			tokio::time::sleep(Duration::from_millis(500)).await;
			ctx.generate_blocks(1).await;
			rebuilt(ctx, srv, db).await;
			assert!(ctx.bitcoind().get_block_count().await - start <= progress_budget);
		}
		panic!("native allocations did not publish within20 bounded progress blocks");
	}).await.expect("bounded automatic expiry publication");
	ctx.generate_blocks(6).await;
	rebuilt(ctx, srv, db).await;
	let mut result = Epoch { parents: BTreeMap::new(), children: BTreeMap::new(),
		outputs: BTreeMap::new(), hashes: BTreeMap::new() };
	for row in rows {
		let id = row.get::<_, &str>("vtxo_id").parse::<VtxoId>().unwrap();
		let leaf = leaves.iter().find(|leaf| leaf.coin.id() == id).unwrap();
		assert_eq!(row.get::<_, i64>("gross_sat"), leaf.coin.amount().to_sat() as i64);
		assert_eq!(row.get::<_, i64>("fee_sat"), leaf.fee as i64);
		assert_eq!(row.get::<_, i64>("payout_sat"), leaf.payout as i64);
		assert_eq!(row.get::<_, i64>("forfeited_sat"), leaf.forfeited as i64);
		assert_eq!(row.get::<_, String>("spk"), leaf.script.as_bytes().to_lower_hex_string());
		assert_eq!(row.get::<_, String>("owner_mailbox"), leaf.mailbox.as_pubkey().to_string());
		assert_eq!(row.get::<_, String>("user_pubkey"), leaf.coin.user_pubkey().to_string());
		let parent = deserialize::<Transaction>(row.get("sweep_tx")).unwrap();
		let txid = parent.compute_txid();
		assert_eq!(row.get::<_, String>("sweep_txid"), txid.to_string());
		assert_eq!(ctx.bitcoind().await_transaction(txid).await, parent);
		let info = core.custom_get_raw_transaction_info(txid, None).unwrap().unwrap();
		assert!(info.confirmations.unwrap_or(0) >= 6);
		let hash = info.blockhash.unwrap();
		let height = bitcoin_ext::BlockHeight::new(u32::try_from(core.get_block_header_info(&hash).unwrap().height).unwrap());
		assert_eq!(core.get_block_hash(u64::from(height.to_u32())).unwrap(), hash);
		let path = std::iter::once(leaf.coin.chain_anchor()).chain(leaf.coin.transactions().map(|step|
			OutPoint::new(step.tx.compute_txid(), step.output_idx as u32))).collect::<BTreeSet<_>>();
		let clause = ark::tree::signed::expiry_clause(leaf.coin.server_pubkey(), leaf.coin.expiry_height());
		assert!(parent.input.iter().any(|input| path.contains(&input.previous_output)
			&& input.witness.taproot_leaf_script().is_some_and(|script|
				script.version == bitcoin::taproot::LeafVersion::TapScript && script.script == clause.as_script())));
		// A zero-net allocation has no output; a positive remainder gets a
		// standard owner output when the scheduled fee can be reduced.
		let index = parent.output.iter().position(|output| output.script_pubkey == leaf.script)
			.map(|index| u32::try_from(index).unwrap());
		if leaf.payout == 0 {
			assert!(index.is_none());
		} else {
			let index = index.unwrap();
			assert_eq!(parent.output[index as usize], TxOut { value: sat(leaf.group_payout), script_pubkey: leaf.script.clone() });
		}
		result.outputs.insert(id, index.map(|index| OutPoint::new(txid, index)));
		result.hashes.insert(txid, hash);
		result.parents.insert(txid, parent);
	}
	for (txid, parent) in &result.parents {
		assert_eq!(parent.version.0, 2);
		assert!(actual_fee(ctx, parent).await > Amount::ZERO);
		let hash = result.hashes[txid];
		let block = core.get_block(&hash).unwrap();
		assert!(block.txdata.iter().any(|transaction| transaction == parent));
		assert!(parent.output.iter().all(|output| !output.is_p2a_fee_anchor()));
	}
	assert!(ctx.bitcoind().get_block_count().await - start <= final_budget);
	result
}

pub(super) async fn receipt(wallet: &Wallet, leaf: &ExpectedLeaf, epoch: &Epoch) -> Option<OutPoint> {
	let point = epoch.outputs[&leaf.coin.id()];
	assert_eq!(leaf.coin.amount(), sat(leaf.fee + leaf.payout + leaf.forfeited));
	if let Some(point) = point {
		let parent = &epoch.parents[&point.txid];
		let output = &parent.output[point.vout as usize];
		assert_eq!(output.script_pubkey, leaf.script);
		assert_eq!(output.value, sat(leaf.group_payout));
		assert_eq!(wallet.get_vtxo_by_id(leaf.coin.id()).await.unwrap().fallback_status,
			Some(bark::vtxo::FallbackStatus::Spent));
	} else {
		assert_eq!(leaf.group_payout, 0);
		assert!(epoch.parents.values().all(|parent| parent.output.iter().all(|out| out.script_pubkey != leaf.script)));
	}
	assert_eq!(wallet.get_vtxo_by_id(leaf.coin.id()).await.unwrap().state, VtxoState::Spent);
	assert_eq!(wallet.get_full_vtxo(leaf.coin.id()).await.unwrap().serialize(), leaf.coin.serialize());
	point
}

pub(super) async fn confirm_actual_path(ctx: &TestContext, leaf: &Vtxo<Full>, seen: &mut BTreeSet<bitcoin::Txid>) {
	for step in leaf.transactions() {
		let txid = step.tx.compute_txid();
		if !seen.insert(txid) { continue; }
		if ctx.bitcoind().sync_client().custom_get_raw_transaction_info(txid, None).unwrap()
			.is_some_and(|info| info.confirmations.is_some_and(|depth| depth > 0))
		{ continue; }
		ctx.broadcast_cpfp(&step.tx).await;
		ctx.generate_blocks(1).await;
	}
	assert!(ctx.bitcoind().sync_client().get_tx_out(&leaf.point().txid, leaf.point().vout, Some(true))
		.unwrap().is_some_and(|output| output.confirmations > 0));
}

pub(super) async fn wait_payout(db: &Db, coin: ark::VtxoId) -> (i64, Transaction) {
	tokio::time::timeout(Duration::from_secs(30), async {
		loop {
			let row = db.read(async |t| Ok(t.query_opt(
				"SELECT a.id,a.sweep_tx FROM expiry_allocation_state a \
				JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
				JOIN expiry_entitlement e ON e.id=c.entitlement_id \
				WHERE e.vtxo_id=$1 AND a.broadcast_at IS NOT NULL ORDER BY a.id LIMIT 1",
				&[&coin.to_string()],
			).await?)).await.unwrap();
			if let Some(row) = row {
				break (row.get("id"), deserialize(row.get::<_, &[u8]>("sweep_tx")).unwrap());
			}
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	}).await.expect("actual background payout did not publish")
}

/// Observe one current customer payout; never manufacture its allocation.
pub(super) async fn published_payout(db: &Db, coin: VtxoId) -> Option<(i64, Transaction)> {
	let rows = db.read(async |t| Ok(t.query("SELECT a.id,a.sweep_tx FROM expiry_allocation_state a
		JOIN expiry_allocation_coin c ON c.allocation_id=a.id JOIN expiry_entitlement e ON e.id=c.entitlement_id
		WHERE e.vtxo_id=$1 AND c.disposition='customer' AND a.status<>'conflicted' AND a.broadcast_at IS NOT NULL",
		&[&coin.to_string()]).await?)).await.unwrap();
	assert!(rows.len() <= 1, "one source cannot have duplicate current payouts");
	rows.first().map(|row| (row.get("id"), deserialize(row.get("sweep_tx")).unwrap()))
}

/// Authenticate each confirmed native timeout or funded operator closure before
/// its canonical height is used as a sender-refund premise.
pub(super) async fn confirmed_grant_closures(
	ctx: &TestContext, db: &Db, grants: &[Vtxo<Full>], receiver: &bitcoin::Script,
) -> Option<std::collections::BTreeMap<VtxoId, (bitcoin::Txid, u32)>> {
	let chain = ctx.bitcoind().sync_client();
	let mut closures = std::collections::BTreeMap::new();
	for grant in grants {
		let row = db.read(async |t| Ok(t.query_one("SELECT v.onchain_spent_txid,h.chain_resolution::TEXT, \
			(SELECT a.sweep_txid FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id \
			JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=v.vtxo_id \
			AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='operator' AND a.status='confirmed') AS swept, \
			h.payment_hash,h.direction::TEXT AS direction \
			FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id WHERE v.vtxo_id=$1", &[&grant.id().to_string()]).await?)).await.unwrap();
		assert_eq!(row.get::<_, &str>("payment_hash"), grant.policy().as_server_htlc_recv().unwrap().payment_hash.to_string());
		assert_eq!(row.get::<_, &str>("direction"), "outgoing");
		let revoked = row.get::<_, Option<&str>>("chain_resolution") == Some("revoked");
		let txid: bitcoin::Txid = match (row.get::<_, Option<&str>>("chain_resolution"), row.get::<_, Option<&str>>("swept")) {
			(Some("revoked"), swept) => {
				let txid = row.get::<_, Option<&str>>("onchain_spent_txid")
					.expect("a native timeout resolution must retain its actual transaction").parse().unwrap();
				if let Some(swept) = swept { assert_eq!(txid, swept.parse::<bitcoin::Txid>().unwrap()); }
				txid
			},
			(None, Some(txid)) => txid.parse().unwrap(),
			(None, None) => return None,
			other => panic!("grant {} has an unexpected closure: {other:?}", grant.id()),
		};
		let Some(info) = chain.custom_get_raw_transaction_info(txid, None).unwrap() else { return None; };
		if info.confirmations.unwrap_or(0) <= 0 { return None; }
		let tx = ctx.bitcoind().await_transaction(txid).await;
		if revoked {
			let input = tx.input.iter().find(|i| i.previous_output == grant.point()).unwrap();
			let policy = grant.policy().as_server_htlc_recv().unwrap();
			let script = policy.server_claim_after_expiry_clause(grant.server_pubkey(), grant.exit_delta()).tapscript();
			assert_eq!(input.witness.taproot_leaf_script().unwrap().script, script.as_script());
		}
		assert!(tx.output.iter().all(|o| o.script_pubkey.as_script() != receiver));
		let hash = info.blockhash.expect("a confirmed grant closure must have a block");
		let height = chain.get_block_header_info(&hash).unwrap().height;
		assert_eq!(chain.get_block_hash(height as u64).unwrap(), hash);
		assert!(closures.insert(grant.id(), (txid, u32::try_from(height).unwrap())).is_none());
	}
	Some(closures)
}

pub(super) async fn assert_grants_reclaimed(ctx: &TestContext, db: &Db, grants: &[Vtxo<Full>], receiver: &bitcoin::Script) {
	confirmed_grant_closures(ctx, db, grants, receiver).await
		.expect("every grant must have a confirmed native timeout or operator closure");
}
