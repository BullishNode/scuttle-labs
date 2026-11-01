//! Native accepted-forfeit versus payout-decision ordering.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ark::{ProtocolEncoding, Vtxo};
use ark::rounds::RoundEvent;
use ark::attestations::RoundParticipationStatusAttestation;
use ark_testing::{btc, sat, TestContext};
use ark_testing::constants::ROUND_CONFIRMATIONS;
use ark_testing::daemon::captaind::{self, ArkClient};
use ark_testing::util::FutureExt;
use bitcoin::{Amount, Transaction};
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use futures::StreamExt;
use serde_json::json;
use server::database::{Db, Tx};
use server_rpc::protos;
use tokio::sync::{Notify, oneshot};

#[derive(Clone)]
struct HoldForfeit {
	arrived: Arc<Mutex<Option<oneshot::Sender<protos::ForfeitVtxosRequest>>>>,
	release: Arc<Notify>,
	response: Arc<Mutex<Option<Result<protos::ForfeitVtxosResponse, tonic::Status>>>>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for HoldForfeit {
	async fn forfeit_vtxos(&self, upstream: &mut ArkClient, request: protos::ForfeitVtxosRequest)
		-> Result<protos::ForfeitVtxosResponse, tonic::Status>
	{
		let arrived = self.arrived.lock().unwrap().take();
		if let Some(arrived) = arrived {
			arrived.send(request.clone()).expect("controller remains alive");
			self.release.notified().await;
		}
		let result = upstream.forfeit_vtxos(request).await.map(tonic::Response::into_inner);
		self.response.lock().unwrap().get_or_insert_with( || result.clone());
		result
	}
}

async fn wait_for_query_lock(tx: &Tx<'_>, prefix: &str) -> anyhow::Result<()> {
	tokio::time::timeout(Duration::from_secs(15), async {
		loop {
			tx.query_one("
				SELECT pg_stat_clear_snapshot()
			", &[]).await?;
			let waiting = tx.query_one("
				SELECT EXISTS(
					SELECT 1
					FROM pg_stat_activity
					WHERE datname=current_database()
					AND wait_event_type='Lock'
					AND ltrim(query, chr(9) || chr(10) || chr(13) || ' ') LIKE $1)
			", &[&prefix]).await?.get::<_, bool>(0);
			if waiting { return Ok::<_, anyhow::Error>(()); }
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	}).await.expect("actual competing operation never reached its source row lock")
}

async fn refresh_completion_race(forfeit_wins: bool) {
	let name = if forfeit_wins { "server/expiry_native_forfeit_wins" }
		else { "server/expiry_native_refresh_decision_wins" };
	let ctx = TestContext::new(name).await;
	let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
		.funded(btc(1)).cfg(|cfg| {
			cfg.vtxo_lifetime = BlockDelta::new(40);
			cfg.vtxo_exit_delta = BlockDelta::new(2);
		})
		.watchmand().create().await;
	assert_eq!(srv.config().fees.refresh.base_fee, Amount::ZERO);
	assert!(srv.config().fees.refresh.ppm_expiry_table.is_empty());
	let (send, arrived) = oneshot::channel();
	let held = HoldForfeit {
		arrived: Arc::new(Mutex::new(Some(send))),
		release: Arc::new(Notify::new()), response: Arc::new(Mutex::new(None))
	};
	let proxy = srv.start_proxy_no_mailbox(held.clone()).await;
	// The fixture owns the precise round, expiry and forfeiture schedule.
	let wallet = Arc::new(ctx.bark_sdk("owner", &proxy)
		.cfg(|cfg| {
			cfg.daemon_manual_sync = true;
			// This fixture deliberately holds one real RPC through backing expiry.
			cfg.daemon_sync_interval_secs = 60;
		}).boarded(sat(100_000)).create().await);
	let original = wallet.spendable_vtxos().await.unwrap().remove(0);
	let destination = wallet.fallback_script();
	let funding_address = srv.watchmand().wait_wallet_address().await;
	ctx.bitcoind().fund_addr(funding_address, sat(100_000)).await;
	ctx.generate_blocks(2).await;
	wallet.refresh_vtxos_delegated([original.id()]).await.unwrap().unwrap();
	let mut finished = srv.subscribe_log::<server_log::RoundFinished>();
	srv.trigger_round().await;
	finished.recv().wait(Duration::from_secs(30)).await.unwrap();
	ctx.generate_blocks(ROUND_CONFIRMATIONS).await;
	wallet.chain().invalidate_caches().await;
	let syncing_wallet = wallet.clone();
	let syncing = tokio::spawn(async move { syncing_wallet.sync_pending_rounds().await });
	let _request = tokio::time::timeout(Duration::from_secs(15), arrived).await.unwrap().unwrap();
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	let (participation, hash) = db.read(async |tx| {
		let row = tx.query_one("
			SELECT p.id, p.unlock_hash
			FROM round_participation p
			JOIN round_part_input i ON i.participation_id=p.id
			WHERE i.vtxo_id=$1
			AND p.round_id IS NOT NULL
			ORDER BY p.id DESC
			LIMIT 1
		", &[&original.id().to_string()]).await?;
		Ok((row.get::<_, i64>("id"), row.get::<_, String>("unlock_hash")))
	}).await.unwrap();
	let hash: ark::tree::signed::UnlockHash = hash.parse().unwrap();
	let status_request = protos::RoundParticipationStatusRequest {
		unlock_hash: hash.to_byte_array().to_vec(),
		attestation: RoundParticipationStatusAttestation::new(hash, &wallet.mailbox_keypair()).serialize(),
	};
	let mut rpc = srv.get_public_rpc().await;
	let issued = rpc.round_participation_status(status_request.clone()).await.unwrap().into_inner();
	assert!(issued.unlock_preimage.is_none());
	let funding: Transaction = deserialize(issued.round_funding_tx.as_ref().unwrap()).unwrap();
	let mut replacements: Vec<Vtxo> = issued.output_vtxos.iter().map(|bytes| Vtxo::deserialize(bytes).unwrap()).collect::<Vec<_>>();
	assert_eq!(ctx.bitcoind().await_transaction(funding.compute_txid()).await, funding);
	let anchor = replacements[0].chain_anchor();
	assert!(ctx.bitcoind().sync_client().get_tx_out(&anchor.txid, anchor.vout, Some(true))
		.unwrap().unwrap().confirmations >= 6);
	assert_eq!(replacements.iter().map(|coin| coin.amount()).sum::<Amount>(), original.amount());
	assert!(replacements.iter().all(|coin| coin.expiry_height() > original.expiry_height()));
	srv.watchmand().stop().await.unwrap();
	let current = ctx.bitcoind().get_block_count().await as u32;
	let expired = ctx.generate_blocks(original.expiry_height().to_u32() + 1 - current).await;
	srv.wait_for_sync_height(expired).await;
	if forfeit_wins {
		db.write(async |tx| {
			tx.query_one("
				SELECT vtxo_id
				FROM vtxo
				WHERE vtxo_id=$1
				FOR UPDATE
			", &[&original.id().to_string()]).await?;
			held.release.notify_one();
			wait_for_query_lock(tx, "SELECT vtxo_id FROM vtxo WHERE vtxo_id = ANY%").await?;
			srv.watchmand().start().await.unwrap();
			wait_for_query_lock(tx, "SELECT vtxo_id,updated_at,spend_state::TEXT,%").await?;
			Ok(())
		}).await.unwrap();
		tokio::time::timeout(Duration::from_secs(20), syncing).await.unwrap().unwrap().unwrap();
		let response = held.response.lock().unwrap().clone().unwrap().unwrap();
		let preimage = response.unlock_preimage.as_slice().try_into().unwrap();
		for coin in &mut replacements {
			assert!(coin.provide_unlock_preimage(preimage));
			// The status tree omits our leaf signature. Validate the
			// signed replacement the SDK durably stored after this exchange.
			let signed = wallet.get_full_vtxo(coin.id()).await.unwrap();
			assert_eq!(signed.amount(), coin.amount());
			assert_eq!(signed.chain_anchor(), coin.chain_anchor());
			signed.validate(&funding).expect("actual accepted-forfeit replacements must be fully signed");
		}
		let released = rpc.round_participation_status(status_request).await.unwrap().into_inner();
		assert_eq!(released.unlock_preimage, Some(response.unlock_preimage));
		let source = db.read(async |tx| tx.get_user_vtxos_by_id(&[original.id()]).await).await.unwrap();
		assert!(source[0].spent_in_round.is_some());
		// A zero customer count before the competing worker finishes would
		// prove nothing. Wait for its operator-only decision and sweep.
		let (allocation, parent): (i64, Transaction) = tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				let row = db.read(async |tx| Ok(tx.query_opt("
					SELECT a.id, a.sweep_tx, c.disposition
					FROM expiry_allocation a
					JOIN expiry_allocation_input i ON i.allocation_id=a.id
					JOIN expiry_allocation_coin c ON c.allocation_id=a.id
					JOIN expiry_entitlement e ON e.id=c.entitlement_id
					WHERE i.vtxo_id=$1
					AND e.vtxo_id=$2
					AND c.active
					AND a.status<>'conflicted'
					AND a.broadcast_at IS NOT NULL
				",
					&[&original.chain_anchor().to_string(), &original.id().to_string()]).await?)).await.unwrap();
				if let Some(row) = row {
					assert_eq!(row.get::<_, String>("disposition"), "operator");
					break (row.get("id"), deserialize(row.get("sweep_tx")).unwrap());
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the competing payout worker must finish the accepted old source as operator-owned");
		assert!(parent.input.iter().any(|input| input.previous_output == original.chain_anchor()));
		assert!(parent.output.iter().all(|output| output.script_pubkey != destination));
		assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
		ctx.generate_blocks(6).await;
		// Core 31 reports the P2A output as `anchor`, which the older
		// verbose RPC model does not recognize. Read Core's depth.
		let confirmed: serde_json::Value = ctx.bitcoind().sync_client().call(
			"getrawtransaction", &[json!(parent.compute_txid().to_string()), json!(true)],
		).unwrap();
		assert!(confirmed["confirmations"].as_u64().unwrap() >= 6);
		tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				let row = db.read(async |tx| Ok(tx.query_one("
					SELECT status
					FROM expiry_allocation
					WHERE id=$1
				", &[&allocation]).await?)).await.unwrap();
				if row.get::<_, String>("status") == "confirmed" { break; }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the actual operator sweep must be projected as confirmed before checking duplicate entitlement");
		let no_duplicate = db.read(async |tx| Ok(tx.query_one("
			SELECT COUNT(*)
			FROM expiry_allocation_coin c
			JOIN expiry_entitlement e ON e.id=c.entitlement_id
			WHERE e.vtxo_id=$1
			AND c.disposition='customer'
		", &[&original.id().to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(no_duplicate, 0, "accepted refresh cannot also pay the original source by fallback");
		let cancelled = db.read(async |tx| Ok(tx.query_one("
			SELECT COUNT(*)
			FROM expiry_cancelled_participation
			WHERE participation_id=$1
		", &[&participation]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(cancelled, 0);
	} else {
		srv.watchmand().start().await.unwrap();
		let parent: Transaction = tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				let row = db.read(async |tx| Ok(tx.query_opt("
					SELECT a.sweep_tx
					FROM expiry_allocation a
					JOIN expiry_allocation_coin c ON c.allocation_id=a.id
					JOIN expiry_entitlement e ON e.id=c.entitlement_id
					WHERE e.vtxo_id=$1
					AND c.active
					AND a.broadcast_at IS NOT NULL
				", &[&original.id().to_string()]).await?)).await.unwrap();
				if let Some(row) = row { break deserialize(row.get("sweep_tx")).unwrap(); }
				tokio::time::sleep(Duration::from_millis(25)).await;
			}
		}).await.expect("actual decision must cancel the nonbinding exchange and pay its original source");
		held.release.notify_one();
		let _client_result = tokio::time::timeout(Duration::from_secs(20), syncing).await.unwrap().unwrap();
		let refusal = held.response.lock().unwrap().clone().unwrap().unwrap_err();
		assert_eq!(refusal.code(), tonic::Code::InvalidArgument);
		let cancelled = db.read(async |tx| {
			let row = tx.query_one("
				SELECT EXISTS(
					SELECT 1
					FROM expiry_cancelled_participation
					WHERE participation_id=$1), EXISTS(
					SELECT 1
					FROM round_part_input
					WHERE participation_id=$1
					AND signed_forfeit_tx IS NOT NULL), EXISTS(
					SELECT 1
					FROM round_participation
					WHERE id=$1
					AND forfeited_at IS NOT NULL)
			", &[&participation]).await?;
			Ok((row.get::<_, bool>(0), row.get::<_, bool>(1), row.get::<_, bool>(2)))
		}).await.unwrap();
		assert_eq!(cancelled, (true, false, false));
		assert_eq!(rpc.round_participation_status(status_request).await.unwrap_err().code(), tonic::Code::NotFound);
		assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
		let (index, output) = parent.output.iter().enumerate().find(|(_, output)| output.script_pubkey == destination).unwrap();
		assert_eq!(output.value, original.amount() - sat(1_000));
		ctx.generate_blocks(6).await;
		assert!(ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), index as u32, Some(true)).unwrap().unwrap().confirmations >= 6);
		let replacement_ids = replacements.iter().map(|coin| coin.id().to_string()).collect::<Vec<_>>();
		let duplicate = db.read(async |tx| Ok(tx.query_one("
			SELECT COUNT(*)
			FROM expiry_allocation_coin c
			JOIN expiry_entitlement e ON e.id=c.entitlement_id
			WHERE e.vtxo_id=ANY($1)
			AND c.disposition='customer'
		", &[&replacement_ids]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(duplicate, 0, "the cancelled exchange cannot issue another customer entitlement");
	}
}

#[tokio::test]
async fn expiry_payout_decision_wins_over_prepared_native_forfeit() {
	refresh_completion_race(false).await;
}

#[tokio::test]
async fn expiry_accepted_native_forfeit_wins_over_waiting_decision() {
	refresh_completion_race(true).await;
}

#[derive(Clone)]
struct HoldRoundSignatures {
	arrived: Arc<Mutex<Option<oneshot::Sender<protos::VtxoSignaturesRequest>>>>,
	release: Arc<Notify>,
	response: Arc<Mutex<Option<Result<protos::Empty, tonic::Status>>>>,
	forfeit_requests: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl captaind::proxy::ArkRpcProxy for HoldRoundSignatures {
	async fn provide_vtxo_signatures(&self, upstream: &mut ArkClient, request: protos::VtxoSignaturesRequest)
		-> Result<protos::Empty, tonic::Status>
	{
		let arrived = self.arrived.lock().unwrap().take();
		if let Some(arrived) = arrived {
			arrived.send(request.clone()).expect("controller remains alive");
			self.release.notified().await;
		}
		let result = upstream.provide_vtxo_signatures(request).await.map(tonic::Response::into_inner);
		*self.response.lock().unwrap() = Some(result.clone());
		result
	}

	async fn forfeit_vtxos(&self, upstream: &mut ArkClient, request: protos::ForfeitVtxosRequest)
		-> Result<protos::ForfeitVtxosResponse, tonic::Status>
	{
		self.forfeit_requests.fetch_add(1, Ordering::SeqCst);
		Ok(upstream.forfeit_vtxos(request).await?.into_inner())
	}
}

#[tokio::test]
async fn expiry_decision_during_round_signing_is_recoverable() {
	Box::pin(async {
		let ctx = TestContext::new("server/expiry_pre_persist_round_decision").await;
		let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).no_vtxo_pool()
			.funded(btc(1)).cfg(|cfg| {
				cfg.vtxo_lifetime = BlockDelta::new(40);
				cfg.vtxo_exit_delta = BlockDelta::new(2);
			}).watchmand().create().await;
		assert_eq!(srv.config().round_sign_time, Duration::from_secs(5));
		assert_eq!(srv.config().fees.refresh.base_fee, Amount::ZERO);
		assert!(srv.config().fees.refresh.ppm_expiry_table.is_empty());
		let (send, arrived) = oneshot::channel();
		let held = HoldRoundSignatures {
			arrived: Arc::new(Mutex::new(Some(send))), release: Arc::new(Notify::new()),
			response: Arc::new(Mutex::new(None)), forfeit_requests: Arc::new(AtomicUsize::new(0)),
		};
		let proxy = srv.start_proxy_no_mailbox(held.clone()).await;
		let wallet = Arc::new(Box::pin(ctx.bark_sdk("owner", &proxy)
			.cfg(|cfg| cfg.daemon_manual_sync = true).boarded(sat(100_000)).create()).await);
		let source = wallet.spendable_vtxos().await.unwrap().remove(0);
		let signed = wallet.get_full_vtxo(source.id()).await.unwrap();
		wallet.validate_vtxo(&signed).await.unwrap();
		let destination = wallet.fallback_script();
		let mailbox = wallet.mailbox_keypair().public_key().to_string();
		let sponsor = srv.watchmand().wait_wallet_address().await;
		ctx.bitcoind().fund_addr(sponsor, sat(100_000)).await;
		ctx.generate_blocks(2).await;
		let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
		// Leave only one block to maturity after trigger_round mines its normal block.
		// The real signature window remains the original five seconds.
		let before = source.expiry_height().to_u32() - 2;
		let current = ctx.bitcoind().get_block_count().await as u32;
		assert!(before >= current);
		let height = ctx.generate_blocks(before - current).await;
		srv.wait_for_sync_height(height).await;
		srv.watchmand().wait_for_sync_height(height).await;
		let initial = db.read(async |tx| tx.get_user_vtxo_by_id(source.id()).await).await.unwrap();
		assert!(!initial.decided_for_fallback);
		assert!(initial.spent_in_round.is_none());
		let mut errors = srv.subscribe_log::<server_log::RoundError>();
		let mut signed_trees = srv.subscribe_log::<server_log::CreatedSignedVtxoTree>();
		let mut accepted_sigs = srv.subscribe_log::<server_log::ReceivedRoundVtxoSignatures>();
		let mut fatal = srv.subscribe_log::<server_log::FatalStoringRound>();
		let mut broadcasts = srv.subscribe_log::<server_log::BroadcastRoundFundingTx>();
		let mut empty = srv.subscribe_log::<server_log::NoRoundPayments>();
		let mut events = wallet.subscribe_round_events().await.unwrap();
		let refreshing_wallet = wallet.clone();
		let source_id = source.id();
		let refreshing = tokio::spawn(async move {
			Box::pin(refreshing_wallet.refresh_vtxos([source_id])).await
		});
		tokio::time::sleep(Duration::from_secs(2)).await;
		srv.trigger_round().await;
		let request = arrived.wait(Duration::from_secs(15)).await.unwrap();
		let proposal = tokio::time::timeout(Duration::from_secs(15), async {
			loop {
				if let RoundEvent::VtxoProposal(proposal) = events.next().await.unwrap().unwrap() {
					break proposal;
				}
			}
		}).await.expect("the held signatures must belong to an actual round proposal");
		let cosigner = bitcoin::secp256k1::PublicKey::from_slice(&request.pubkey).unwrap();
		let outputs = proposal.vtxos_spec.vtxos.iter()
			.filter(|leaf| leaf.cosign_pubkey == Some(cosigner)).collect::<Vec<_>>();
		assert_eq!(outputs.len(), 1);
		assert_eq!(outputs[0].vtxo.amount, source.amount());
		assert!(!request.signatures.is_empty());
		let funding_id = proposal.unsigned_round_tx.compute_txid();
		let seq = i64::try_from(proposal.round_seq.inner()).unwrap();
		let no_persistence = || async {
			db.read(async |tx| {
				let row = tx.query_one("SELECT
					(SELECT count(*) FROM round WHERE seq=$1 OR funding_txid=$2),
					(SELECT count(*) FROM round_part_input WHERE vtxo_id=$3),
					(SELECT count(*) FROM virtual_transaction WHERE txid=$2)",
					&[&seq, &funding_id.to_string(), &source.id().to_string()]).await?;
				Ok((row.get::<_, i64>(0), row.get::<_, i64>(1), row.get::<_, i64>(2)))
			}).await.unwrap()
		};
		assert_eq!(no_persistence().await, (0, 0, 0));
		let checkpoints = wallet.pending_round_states().await.unwrap();
		assert_eq!(checkpoints.len(), 1);
		assert_eq!(checkpoints[0].state().participation().inputs.iter().map(|coin| coin.id()).collect::<Vec<_>>(), vec![source.id()]);
		assert!(!checkpoints[0].state().forfeits_may_be_exposed());
		assert_eq!(held.forfeit_requests.load(Ordering::SeqCst), 0);
		let expired = ctx.generate_blocks(1).await;
		assert_eq!(expired, source.expiry_height());
		srv.wait_for_sync_height(expired).await;
		srv.watchmand().wait_for_sync_height(expired).await;
		srv.watchmand().trigger_sweep().await;
		let allocation = tokio::time::timeout(Duration::from_secs(15), async {
			loop {
				let row = db.read(async |tx| Ok(tx.query_opt("SELECT a.id, c.owner_mailbox,
					c.user_pubkey,c.gross_sat,c.fee_sat,c.payout_sat,c.forfeited_sat,c.spk
					FROM expiry_allocation a JOIN expiry_allocation_coin c ON c.allocation_id=a.id
					JOIN expiry_entitlement e ON e.id=c.entitlement_id
					WHERE e.vtxo_id=$1 AND c.active AND c.disposition='customer' AND a.status<>'conflicted'",
					&[&source.id().to_string()]).await?)).await.unwrap();
				if let Some(row) = row { break row; }
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		}).await.expect("MISSED_TARGET: real watchmand did not commit the source decision before signature release");
		assert_eq!(allocation.get::<_, String>("owner_mailbox"), mailbox);
		assert_eq!(allocation.get::<_, String>("user_pubkey"), source.policy().user_pubkey().to_string());
		assert_eq!(allocation.get::<_, i64>("gross_sat"), 100_000);
		assert_eq!(allocation.get::<_, i64>("fee_sat"), 1_000);
		assert_eq!(allocation.get::<_, i64>("payout_sat"), 99_000);
		assert_eq!(allocation.get::<_, i64>("forfeited_sat"), 0);
		assert_eq!(allocation.get::<_, String>("spk"), destination.to_hex_string());
		let before_release_rows = no_persistence().await;
		assert_eq!(before_release_rows, (0, 0, 0));
		let decided = db.read(async |tx| tx.get_user_vtxo_by_id(source.id()).await).await.unwrap();
		assert!(decided.decided_for_fallback);
		assert!(decided.spent_in_round.is_none());
		tokio::fs::write(ctx.datadir.join("a6-before-signature-release.json"), serde_json::to_vec_pretty(&json!({
			"source": source.id(), "round_seq": proposal.round_seq, "attempt_seq": proposal.attempt_seq,
			"proposed_funding": funding_id, "allocation": allocation.get::<_, i64>("id"),
			"round_input_funding_rows": before_release_rows, "forfeit_requests": held.forfeit_requests.load(Ordering::SeqCst),
		})).unwrap()).await.unwrap();
		// Forward the identical genuine signatures; no error or outcome is synthesized.
		held.release.notify_one();
		let valid = accepted_sigs.recv().wait(Duration::from_secs(15)).await.unwrap();
		assert_eq!(valid.round_seq, proposal.round_seq, "MISSED_TARGET: original round timed out before the decision");
		assert_eq!(valid.attempt_seq, proposal.attempt_seq);
		let combined = signed_trees.recv().wait(Duration::from_secs(15)).await.unwrap();
		assert_eq!(combined.round_seq, proposal.round_seq);
		let recoverable = errors.recv().wait(Duration::from_secs(15)).await.unwrap();
		assert_eq!(recoverable.round_seq, proposal.round_seq);
		assert_eq!(recoverable.error, format!("tx body error: {}", server::FallbackRefusal::DecidedForFallback(source.id())));
		// This typed log is emitted ONLY by RoundError::Recoverable, not by a
		// generic Failed event, timeout or FatalStoringRound. The scheduler still
		// waits for its normal next trigger; request it without mining another block.
		srv.get_round_rpc().await.trigger_round(protos::Empty {}).await.unwrap();
		let continued = empty.recv().wait(Duration::from_secs(15)).await.unwrap();
		assert!(continued.round_seq > proposal.round_seq);
		let client_error = refreshing.wait(Duration::from_secs(15)).await.unwrap().unwrap_err();
		assert!(client_error.to_string().contains("round failed"));
		assert!(held.response.lock().unwrap().as_ref().unwrap().is_ok());
		assert!(fatal.try_recv().is_err());
		assert!(broadcasts.try_recv().is_err());
		assert_eq!(no_persistence().await, (0, 0, 0));
		assert!(ctx.bitcoind().sync_client().custom_get_raw_transaction_info(funding_id, None).unwrap().is_none());
		assert_eq!(held.forfeit_requests.load(Ordering::SeqCst), 0);
		assert!(wallet.pending_round_states().await.unwrap().is_empty());
		assert_eq!(wallet.get_full_vtxo(source.id()).await.unwrap().serialize(), signed.serialize());
		let allocation_id: i64 = allocation.get("id");
		let parent: Transaction = tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				let tx = db.read(async |t| Ok(t.query_opt("SELECT sweep_tx FROM expiry_allocation
					WHERE id=$1 AND broadcast_at IS NOT NULL", &[&allocation_id]).await?)).await.unwrap();
				if let Some(tx) = tx { break deserialize(tx.get("sweep_tx")).unwrap(); }
				tokio::time::sleep(Duration::from_millis(25)).await;
			}
		}).await.unwrap();
		assert!(parent.input.iter().any(|input| input.previous_output == source.chain_anchor()));
		assert_eq!(ctx.bitcoind().await_transaction(parent.compute_txid()).await, parent);
		let paid = parent.output.iter().enumerate().filter(|(_, output)| output.script_pubkey == destination).collect::<Vec<_>>();
		assert_eq!(paid.len(), 1);
		assert_eq!(paid[0].1.value, sat(99_000));
		let confirmed = ctx.generate_blocks(6).await;
		srv.wait_for_sync_height(confirmed).await;
		srv.watchmand().wait_for_sync_height(confirmed).await;
		let depth = ctx.bitcoind().sync_client().get_tx_out(&parent.compute_txid(), paid[0].0 as u32, Some(true))
			.unwrap().unwrap().confirmations;
		assert!(depth >= 6);
		tokio::time::timeout(Duration::from_secs(15), async {
			loop {
				wallet.sync().await;
				if wallet.get_vtxo_by_id(source.id()).await.unwrap().fallback_status == Some(bark::vtxo::FallbackStatus::Spent) { break; }
				tokio::time::sleep(Duration::from_millis(25)).await;
			}
		}).await.expect("the canonical payout must retire the original signed coin");
		let retired = wallet.get_vtxo_by_id(source.id()).await.unwrap();
		assert_eq!(retired.fallback_status, Some(bark::vtxo::FallbackStatus::Spent));
		assert_eq!(retired.state, bark::vtxo::VtxoState::Spent);
		assert_eq!(wallet.balance().await.unwrap().total(), Amount::ZERO);
		assert!(wallet.spendable_vtxos().await.unwrap().is_empty());
		let history = wallet.history().await.unwrap();
		for _ in 0..2 { wallet.sync().await; }
		assert_eq!(wallet.history().await.unwrap(), history);
		assert_eq!(no_persistence().await, (0, 0, 0));
		let count = db.read(async |tx| Ok(tx.query_one("SELECT count(*) FROM expiry_allocation_coin c
			JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1 AND c.active",
			&[&source.id().to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		assert_eq!(count, 1);
		tokio::fs::write(ctx.datadir.join("a6-recoverable-result.json"), serde_json::to_vec_pretty(&json!({
			"source": source.id(), "round_seq": proposal.round_seq, "recoverable": recoverable.error,
			"continued_round_seq": continued.round_seq, "proposed_funding": funding_id,
			"allocation": allocation_id, "payout_txid": parent.compute_txid(),
			"gross_sat":100_000, "fee_sat":1_000, "payout_sat":99_000,
			"confirmations":depth, "forfeit_requests": held.forfeit_requests.load(Ordering::SeqCst),
		})).unwrap()).await.unwrap();
	}).await;
}
