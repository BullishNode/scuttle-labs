//! Reconcile a real payment after its caller has stopped, using its own native label.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use ark::lightning::{PaymentHash, Preimage};
use ark::ProtocolEncoding;
use bark::{LightningSendOptions, OpenWalletArgs, Wallet, WalletSeed};
use bark::actions::lightning::pay::{LightningSendState, Progress};
use bark::movement::MovementStatus;
use bark::persist::{BarkPersister, sqlite::SqliteClient};
use bark::subsystem::Subsystem;
use ark_testing::{btc, sat, TestContext};
use ark_testing::daemon::lightningd::Lightningd;
use ark_testing::daemon::captaind::Captaind;
use ark_testing::onchain::AppBip86Wallet;
use bitcoin::{Amount, Network, OutPoint, Transaction, TxOut};
use bitcoin::consensus::deserialize;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi};
use cln_rpc::listpays_pays::ListpaysPaysStatus;
use cln_rpc::plugins::hold;
use server::database::Db;
use server_rpc::protos;

async fn remote_pays(node: &Lightningd, hash: PaymentHash) -> Vec<cln_rpc::ListpaysPays> {
	node.grpc_client().await.list_pays(cln_rpc::ListpaysRequest {
		bolt11: None, payment_hash: Some(hash.to_vec()), status: None,
		index: None, limit: None, start: None,
	}).await.unwrap().into_inner().pays
}

async fn assert_no_success_in_sender_mailbox(
	srv: &Captaind, auth: &ark::mailbox::MailboxAuthorization, hash: PaymentHash,
	expected_terminal_count: usize,
) {
	let mut rpc = srv.get_mailbox_public_rpc().await;
	let mut checkpoint = 0;
	let mut finished = 0;
	for _ in 0..32 {
		let page = rpc.read_mailbox(protos::mailbox_server::MailboxRequest {
			mailbox_id: auth.mailbox().serialize(), authorization: Some(auth.serialize()), checkpoint,
		}).await.unwrap().into_inner();
		for message in page.messages {
			assert!(message.checkpoint > checkpoint);
			checkpoint = message.checkpoint;
			if let Some(protos::mailbox_server::mailbox_message::Message::LightningSendFinished(message)) = message.message {
				if message.payment_hash == hash.to_vec() {
					finished += 1;
					assert!(message.preimage.is_none(), "a later attempt's preimage must not be sent to the failed sender");
				}
			}
		}
		if !page.have_more {
			assert_eq!(finished, expected_terminal_count, "authenticated terminal payload count differs from the native outcome");
			return;
		}
	}
	panic!("the bounded previous mailbox inventory did not finish");
}

async fn reopen_sender(ctx: &TestContext, srv: &Captaind, name: &str, seed: &bip39::Mnemonic) -> Wallet {
	let db: Arc<dyn BarkPersister> = Arc::new(SqliteClient::open(ctx.datadir.join(name).join("db.sqlite")).unwrap());
	let onchain = AppBip86Wallet::load_or_create(Network::Regtest, seed.to_seed(""), db.clone(),
		ctx.bitcoind().sync_client()).await.unwrap();
	let script = onchain.fallback_script();
	let mut config = ctx.bark_default_cfg(srv, Some(ctx.bitcoind()));
	config.daemon_manual_sync = true;
	config.vtxo_refresh_expiry_threshold = BlockDelta::new(4);
	config.vtxo_exit_margin = BlockDelta::new(2);
	Wallet::open(Network::Regtest, WalletSeed::new_from_mnemonic(Network::Regtest, seed), config,
		OpenWalletArgs { persister: Some(db), onchain: Some(Arc::new(tokio::sync::RwLock::new(onchain))), run_daemon: false, ..Default::default() }).await.unwrap()
}

#[tokio::test]
async fn returning_failed_sender_is_not_debited_by_a_later_same_hash_ark_payment() {
	tokio::time::timeout(Duration::from_secs(180), Box::pin(async {
		let ctx = TestContext::new("server/returning_failed_sender_later_same_hash_attempt").await;
		let lightning = ctx.new_lightning_setup("lightningd").await;
		let later_receiver = ctx.lightningd("later-receiver").create().await;
		lightning.internal.connect(&later_receiver).await;
		let channel = lightning.internal.fund_channel(&later_receiver, btc(1)).await;
		ctx.await_transaction(channel).await;
		ctx.generate_blocks(6).await;
		lightning.internal.wait_for_gossip(2).await;
		let srv = ctx.captaind("server").bitcoind(ctx.bitcoind_arc()).lightningd(&lightning.internal)
			.no_vtxo_pool().funded(btc(10)).cfg(|config| { config.expiry_payouts = true;
				config.vtxo_lifetime = BlockDelta::new(24);
				config.vtxo_exit_delta = BlockDelta::new(3);
				config.htlc_send_expiry_delta = BlockDelta::new(40);
				config.invoice_check_interval = Duration::from_secs(1);
			}).watchmand().create().await;
		let start_height = ctx.bitcoind().get_block_count().await;
		let operator_script = {
			let config = srv.watchmand().config();
			config.sweep_address.clone().unwrap().require_network(Network::Regtest).unwrap().script_pubkey()
		};
		let mnemonic = bip39::Mnemonic::generate(12).unwrap();
		let old = Box::pin(ctx.bark_sdk("failed-sender", &srv).mnemonic(mnemonic.clone()).app_bip86()
			.cfg(|config| { config.daemon_manual_sync = true; config.vtxo_refresh_expiry_threshold=BlockDelta::new(4); config.vtxo_exit_margin=BlockDelta::new(2); }).boarded(sat(100_000)).create()).await;
		old.stop_daemon_wait().await.unwrap();
		let destination = old.fallback_script();
		let mailbox = old.mailbox_authorization(chrono::Local::now() + Duration::from_secs(600));
		let preimage = Preimage::random();
		let hash = preimage.compute_payment_hash();
		let mut first_hold = lightning.external.hold_client().await;
		let first_invoice = first_hold.invoice(hold::InvoiceRequest {
			payment_hash: hash.to_vec(), amount_msat: 100_000_000,
			description: Some(hold::invoice_request::Description::Memo("previous native failed sender".into())),
			min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
		}).await.unwrap().into_inner().bolt11;
		lightning.sync().await;
		old.pay_lightning_invoice_with(first_invoice, None,
			LightningSendOptions::default().wait(false).retry_for(Some(Duration::from_secs(1)))).await.unwrap();
		lightning.external.wait_for_hold_invoice_accepted(hash).await;
		let old_send = old.lightning_send_checkpoint(hash).await.unwrap().unwrap();
		let old_htlcs = match &old_send.progress {
			Progress::PaymentInitiated(htlcs) => htlcs.clone(),
			other => panic!("previous send must be durably initiated: {other:?}"),
		};
		assert_eq!(old_htlcs.vtxo_ids.len(), 1);
		assert_eq!(old_htlcs.mailbox_id, old.mailbox_identifier());
		assert_eq!(old_send.total_amount(), sat(100_000));
		let old_source = old.get_full_vtxo(old_htlcs.vtxo_ids[0]).await.unwrap();
		let old_funding = ctx.bitcoind().await_transaction(old_source.chain_anchor().txid).await;
		old_source.validate(&old_funding).unwrap();
		assert_eq!(old.get_vtxo_key(&old_source).await.unwrap().public_key(), old_source.policy().user_pubkey());
		assert_eq!(old_source.amount(), sat(100_000));
		drop(old);
		let db = Db::connect(&srv.config().postgres).await.unwrap();
		let old_attempt = db.read(async |tx| Ok(tx.query_one("SELECT id FROM lightning_payment_attempt WHERE payment_hash=$1",
			&[&hash.to_string()]).await?.get::<_, i64>(0))).await.unwrap();
		let old_label = old_attempt.to_string();
		srv.watchmand().stop().await.unwrap();
		srv.stop().await.unwrap();
		first_hold.cancel(hold::CancelRequest { payment_hash: hash.to_vec() }).await.unwrap();
		let failed_payment = tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				let pays = remote_pays(&lightning.internal, hash).await;
				assert_eq!(pays.len(), 1);
				if pays[0].status() == ListpaysPaysStatus::Failed {
					assert_eq!(pays[0].label.as_deref(), Some(old_label.as_str()));
					assert!(pays[0].preimage.is_none());
					break pays.into_iter().next().unwrap();
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the real previous payment must fail while its wallet and server are absent");
		srv.start().await.unwrap();
		srv.watchmand().start().await.unwrap();
		tokio::time::timeout(Duration::from_secs(45), async {
			loop {
				let status = db.read(async |tx| Ok(tx.query_one("SELECT status::TEXT FROM lightning_payment_attempt WHERE id=$1",
					&[&old_attempt]).await?.get::<_, String>(0))).await.unwrap();
				if status == "failed" {
					// The status commits before the mailbox notification.
					let notified = db.read(async |tx| Ok(tx.query_one("SELECT EXISTS(SELECT 1 FROM mailbox \
						WHERE unblinded_mailbox_id=$1 AND payment_hash=$2 AND mailbox_type='ln-send-finished')",
						&[&mailbox.mailbox().to_string(), &hash.to_string()]).await?.get::<_, bool>(0))).await.unwrap();
					if notified { break; }
				}
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the normal monitor must close the previous failure before a later Ark attempt");
		assert_no_success_in_sender_mailbox(&srv, &mailbox, hash, 1).await;

		let later = Box::pin(ctx.bark_sdk("later-sender", &srv).app_bip86()
			.cfg(|config| { config.daemon_manual_sync = true; config.vtxo_refresh_expiry_threshold=BlockDelta::new(4); config.vtxo_exit_margin=BlockDelta::new(2); }).boarded(sat(50_000)).boarded(sat(50_000)).create()).await;
		later.stop_daemon_wait().await.unwrap();
		let later_destination = later.fallback_script();
		assert_ne!(later_destination, destination);
		let mut later_hold = later_receiver.hold_client().await;
		let later_invoice = later_hold.invoice(hold::InvoiceRequest {
			payment_hash: hash.to_vec(), amount_msat: 100_000_000,
			description: Some(hold::invoice_request::Description::Memo("different native sender and receiver".into())),
			min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
		}).await.unwrap().into_inner().bolt11;
		later_receiver.wait_for_block_sync().await;
		lightning.internal.wait_for_block_sync().await;
		later.pay_lightning_invoice_with(later_invoice, None,
			LightningSendOptions::default().wait(false).retry_for(Some(Duration::from_secs(1)))).await.unwrap();
		later_receiver.wait_for_hold_invoice_accepted(hash).await;
		let later_send = later.lightning_send_checkpoint(hash).await.unwrap().unwrap();
		let later_htlcs = match &later_send.progress {
			Progress::PaymentInitiated(htlcs) => htlcs.clone(),
			other => panic!("later send must use a real different SDK checkpoint: {other:?}"),
		};
		assert_eq!(later_send.total_amount(),sat(100_000));
		assert_eq!(later_send.fee,Amount::ZERO);
		assert_eq!(later_htlcs.vtxo_ids.len(), 2, "the later native send must have a incomplete-scope boundary");
		assert_ne!(later_htlcs.vtxo_ids, old_htlcs.vtxo_ids);
		let mut later_sources = Vec::new();
		let mut later_fundings = Vec::new();
		for id in &later_htlcs.vtxo_ids {
			let source = later.get_full_vtxo(*id).await.unwrap();
			let funding = ctx.bitcoind().await_transaction(source.chain_anchor().txid).await;
			source.validate(&funding).unwrap();
			assert_eq!(later.get_vtxo_key(&source).await.unwrap().public_key(), source.policy().user_pubkey());
			assert_ne!(source.chain_anchor(), old_source.chain_anchor());
			assert_eq!(source.amount(), sat(50_000));
			later_sources.push(source); later_fundings.push(funding);
		}
		assert_ne!(later_sources[0].chain_anchor(),later_sources[1].chain_anchor());
		for (ids,failed) in [(&old_htlcs.vtxo_ids,true),(&later_htlcs.vtxo_ids,false)] {
			let mut sorted=ids.clone(); sorted.sort_unstable();
			let scope=sorted.iter().map(|id|id.to_bytes().to_vec()).collect::<Vec<_>>();
			let status=srv.get_public_rpc().await.check_lightning_payment(protos::CheckLightningPaymentRequest {
				hash:hash.to_vec(),wait:false,htlc_vtxo_ids:scope.clone(),
			}).await.unwrap().into_inner();
			assert_eq!(status.htlc_vtxo_ids,scope);
			if failed { assert!(matches!(status.payment_status,Some(protos::lightning_payment_status::PaymentStatus::Failed(_)))); }
			else { assert!(matches!(status.payment_status,Some(protos::lightning_payment_status::PaymentStatus::Pending(_)))); }
		}
		later_hold.settle(hold::SettleRequest { payment_preimage: preimage.to_vec() }).await.unwrap();
		assert!(matches!(later.check_lightning_payment(hash, true).await.unwrap(), LightningSendState::Paid(paid) if paid.preimage == preimage));
		assert!(later.is_invoice_paid(hash).await.unwrap());
		let later_attempt = db.read(async |tx| tx.get_latest_payment_attempt_by_payment_hash(hash).await).await.unwrap().unwrap();
		assert_ne!(later_attempt.id, old_attempt);
		let later_label = later_attempt.id.to_string();
		assert_ne!(later_label, old_label);
		let mut rpc = srv.get_public_rpc().await;
		for (ids, success) in [(&old_htlcs.vtxo_ids, false), (&later_htlcs.vtxo_ids, true)] {
			let mut ids = ids.clone(); ids.sort_unstable();
			let scope = ids.iter().map(|id|id.to_bytes().to_vec()).collect::<Vec<_>>();
			let status = rpc.check_lightning_payment(protos::CheckLightningPaymentRequest {
				hash: hash.to_vec(), wait: false, htlc_vtxo_ids: scope.clone(),
			}).await.unwrap().into_inner();
			assert_eq!(status.htlc_vtxo_ids, scope);
			if success {
				assert!(matches!(status.payment_status, Some(protos::lightning_payment_status::PaymentStatus::Success(value)) if value.preimage == preimage.to_vec()));
			} else { assert!(matches!(status.payment_status, Some(protos::lightning_payment_status::PaymentStatus::Failed(_)))); }
		}
		for scope in [Vec::new(), vec![old_htlcs.vtxo_ids[0],later_htlcs.vtxo_ids[0]], vec![old_htlcs.vtxo_ids[0],old_htlcs.vtxo_ids[0]], vec![old_send.input_vtxo_ids[0]], vec![later_htlcs.vtxo_ids[0]]] {
			let status = rpc.check_lightning_payment(protos::CheckLightningPaymentRequest {
				hash: hash.to_vec(), wait: false, htlc_vtxo_ids: scope.iter().map(|id|id.to_bytes().to_vec()).collect(),
			}).await.unwrap().into_inner();
			assert!(status.htlc_vtxo_ids.is_empty());
			assert!(matches!(status.payment_status, Some(protos::lightning_payment_status::PaymentStatus::Pending(_))));
		}
		assert_no_success_in_sender_mailbox(&srv, &mailbox, hash, 1).await;
		drop(later);

		// The paid later sources and failed source form two expiry
		// heights. Preserve the same 100,000 sat fee capital, usable independently by
		// both packages before any intervening block confirms their change.
		assert_eq!(later_sources[0].expiry_height(), later_sources[1].expiry_height());
		assert_ne!(old_source.expiry_height(), later_sources[0].expiry_height());
		let fee_address = srv.watchmand().wait_wallet_address().await;
		let mut sponsor_ids = Vec::new();
		for _ in 0..2 {
			sponsor_ids.push(ctx.bitcoind().fund_addr(fee_address.clone(), sat(50_000)).await);
		}
		ctx.generate_blocks(2).await;
		let mut sponsor_roots = BTreeMap::new();
		for sponsor in sponsor_ids {
			let tx = ctx.bitcoind().await_transaction(sponsor).await;
			let outputs = tx.output.iter().enumerate().filter(|(_,out)|out.script_pubkey == fee_address.script_pubkey()).collect::<Vec<_>>();
			assert_eq!(outputs.len(), 1); assert_eq!(outputs[0].1.value,sat(50_000));
			let point = OutPoint::new(sponsor, outputs[0].0 as u32);
			let utxo = ctx.bitcoind().sync_client().get_tx_out(&point.txid,point.vout,Some(true)).unwrap().unwrap();
			assert!(utxo.confirmations >= 2); assert_eq!(utxo.value,sat(50_000));
			assert_eq!(utxo.script_pub_key.script().unwrap(),fee_address.script_pubkey());
			assert!(sponsor_roots.insert(point, outputs[0].1.clone()).is_none());
		}
		assert_eq!(sponsor_roots.len(),2);
		assert_eq!(sponsor_roots.values().map(|out|out.value).sum::<Amount>(),sat(100_000));
		let height = ctx.bitcoind().get_block_count().await as u32;
		assert!(height < old_send.htlc_expiry.to_u32());
		let expired = ctx.generate_blocks(old_send.htlc_expiry.to_u32()+1-height).await;
		srv.wait_for_sync_height(expired).await;
		srv.watchmand().wait_for_sync_height(expired).await;
		let parent = tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				let row = db.read(async |tx| Ok(tx.query_opt("SELECT a.sweep_tx FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='customer' AND a.broadcast_at IS NOT NULL", &[&old_source.id().to_string()]).await?)).await.unwrap();
				if let Some(row) = row { break deserialize::<Transaction>(row.get(0)).unwrap(); }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the absent previous failed sender must receive its fallback despite the later payment");
		ctx.bitcoind().await_transaction(parent.compute_txid()).await;
		let outputs = parent.output.iter().enumerate().filter(|(_,out)|out.script_pubkey==destination).collect::<Vec<_>>();
		assert_eq!(outputs.len(),1); assert_eq!(outputs[0].1.value,sat(99_000));
		let payout = OutPoint::new(parent.compute_txid(),outputs[0].0 as u32);
		let confirmed = ctx.generate_blocks(6).await;
		srv.wait_for_sync_height(confirmed).await;
		srv.watchmand().wait_for_sync_height(confirmed).await;
		let core = ctx.bitcoind().sync_client();
		let utxo = core.get_tx_out(&payout.txid,payout.vout,Some(true)).unwrap().unwrap();
		assert!(utxo.confirmations>=6); assert_eq!(utxo.value,sat(99_000));
		assert_eq!(utxo.script_pub_key.script().unwrap(),destination);
		assert_no_success_in_sender_mailbox(&srv, &mailbox, hash, 1).await;
		tokio::time::timeout(Duration::from_secs(30),async {
			loop {
				let confirmed=db.read(async |tx| Ok(tx.query_opt("SELECT a.id FROM expiry_allocation_state a JOIN expiry_allocation_coin c ON c.allocation_id=a.id JOIN expiry_entitlement e ON e.id=c.entitlement_id WHERE e.vtxo_id=$1 AND EXISTS(SELECT 1 FROM expiry_allocation_state current_allocation WHERE current_allocation.id=c.allocation_id AND current_allocation.status<>'conflicted') AND c.disposition='customer' AND a.status='confirmed' AND a.sweep_txid=$2",&[&old_source.id().to_string(),&parent.compute_txid().to_string()]).await?.is_some())).await.unwrap();
				if confirmed { break; }
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		}).await.expect("the canonical refund must enter confirmed server history before client adoption");
		for _ in 0..2 {
			let returned = Box::pin(reopen_sender(&ctx,&srv,"failed-sender",&mnemonic)).await;
			for _ in 0..2 {
				returned.chain().invalidate_caches().await;
				returned.sync_mailbox().await.unwrap();
				returned.sync().await;
				returned.sync_onchain().await.unwrap();
			}
			assert!(!returned.is_invoice_paid(hash).await.unwrap());
			let onchain = returned.onchain().unwrap();
			assert!(onchain.read().await.is_mine(&destination).await.unwrap());
			assert!(onchain.read().await.balance().await >= sat(99_000), "the onchain wallet must discover its payout");
			let received = core.get_tx_out(&payout.txid,payout.vout,Some(true)).unwrap().unwrap();
			assert_eq!(received.value, sat(99_000));
			assert_eq!(received.script_pub_key.script().unwrap(), destination);
			let history = returned.history().await.unwrap();
			let sends = history.iter().filter(|row|row.subsystem.is_subsystem(Subsystem::LIGHTNING_SEND)).collect::<Vec<_>>();
			assert_eq!(sends.len(),1); assert_eq!(sends[0].id,old_htlcs.movement_id);
			assert_eq!(sends[0].status,MovementStatus::Failed);
			assert_eq!(sends[0].effective_balance.to_sat(),0);
			assert_eq!(sends[0].offchain_fee,Amount::ZERO);
		}

		let mut roots = sponsor_roots.clone();
		assert!(roots.insert(old_source.chain_anchor(),old_funding.output[old_source.chain_anchor().vout as usize].clone()).is_none());
		for (source,funding) in later_sources.iter().zip(&later_fundings) {
			assert!(roots.insert(source.chain_anchor(),funding.output[source.chain_anchor().vout as usize].clone()).is_none());
		}
		assert_eq!(roots.len(),5); assert_eq!(roots.values().map(|out|out.value).sum::<Amount>(),sat(300_000));
		let mut known = roots.clone(); let mut terminal = roots.clone(); let mut graph = BTreeMap::new();
		let mut spent = BTreeSet::new(); let mut miner = Amount::ZERO;
		let mut customer = BTreeSet::new();
		for height in start_height+1..=core.get_block_count().unwrap() {
			for tx in core.get_block(&core.get_block_hash(height).unwrap()).unwrap().txdata {
				if !tx.input.iter().any(|input|known.contains_key(&input.previous_output)) { continue; }
				let mut value = Amount::ZERO;
				for input in &tx.input {
					assert!(spent.insert(input.previous_output));
					value+=known.get(&input.previous_output).expect("no undeclared money may enter the complete backing/sponsor cut").value;
					assert!(terminal.remove(&input.previous_output).is_some());
				}
				miner+=value.checked_sub(tx.output.iter().map(|out|out.value).sum()).unwrap();
				let txid=tx.compute_txid();
				for (vout,out) in tx.output.iter().enumerate() {
					let point=OutPoint::new(txid,vout as u32); known.insert(point,out.clone()); terminal.insert(point,out.clone());
					if out.script_pubkey==destination { customer.insert(point); assert_eq!(out.value,sat(99_000)); }
					assert_ne!(out.script_pubkey,later_destination,"the paid later source must not get a fallback");
				}
				assert!(graph.insert(txid,tx).is_none()); assert!(graph.len()<=64);
			}
		}
		// Every canonical fee child must spend a declared sponsor,
		// never customer principal or an undeclared funding input.
		for tx in graph.values() {
			let anchors = tx.input.iter().filter(|input|known[&input.previous_output].script_pubkey==*bitcoin_ext::fee::P2A_SCRIPT).count();
			if anchors == 0 { continue; }
			assert_eq!(anchors,1);
			let fee_inputs = tx.input.iter().filter(|input|known[&input.previous_output].script_pubkey!=*bitcoin_ext::fee::P2A_SCRIPT).collect::<Vec<_>>();
			assert!(!fee_inputs.is_empty());
			assert!(fee_inputs.iter().all(|input|sponsor_roots.contains_key(&input.previous_output)));
		}
		assert_eq!(customer,BTreeSet::from([payout]));
		assert_eq!(sat(300_000),terminal.values().map(|out|out.value).sum::<Amount>()+miner);
		assert_eq!(graph.get(&parent.compute_txid()),Some(&parent));
		assert_eq!(parent.version.0,3); assert!(parent.vsize()<=10_000);
		let parent_inputs=parent.input.iter().map(|input|known[&input.previous_output].value).sum::<Amount>();
		assert_eq!(parent_inputs,parent.output.iter().map(|out|out.value).sum());
		let anchors=parent.output.iter().enumerate().filter(|(_,out)|out.script_pubkey==*bitcoin_ext::fee::P2A_SCRIPT).collect::<Vec<_>>();
		assert_eq!(anchors.len(),1); assert_eq!(anchors[0].1.value,Amount::ZERO);
		let anchor=OutPoint::new(parent.compute_txid(),anchors[0].0 as u32);
		let children=graph.values().filter(|tx|tx.input.iter().any(|input|input.previous_output==anchor)).collect::<Vec<_>>();
		assert_eq!(children.len(),1); assert_eq!(children[0].version.0,3); assert!(children[0].vsize()<=1_000);
		let parent_info:serde_json::Value=core.call("getrawtransaction",&[serde_json::json!(parent.compute_txid().to_string()),serde_json::json!(true)]).unwrap();
		let child_info:serde_json::Value=core.call("getrawtransaction",&[serde_json::json!(children[0].compute_txid().to_string()),serde_json::json!(true)]).unwrap();
		assert_eq!(parent_info["blockhash"],child_info["blockhash"]); assert!(child_info["confirmations"].as_u64().unwrap()>=6);
		let child_fee=children[0].input.iter().map(|input|known[&input.previous_output].value).sum::<Amount>()-children[0].output.iter().map(|out|out.value).sum::<Amount>();
		assert!(child_fee>Amount::ZERO);
		let admin = srv.watchmand().config().admin_address.unwrap();
		let mut watch = ark_testing::daemon::captaind::WalletAdminClient::connect(format!("http://{admin}")).await.unwrap();
		let watch_points=watch.wallet_status(protos::Empty{}).await.unwrap().into_inner().watchman.unwrap().confirmed_utxos.into_iter().map(|point|point.parse::<OutPoint>().unwrap()).collect::<BTreeSet<_>>();
		for (point,out) in terminal {
			if out.value==Amount::ZERO { continue; }
			let utxo=core.get_tx_out(&point.txid,point.vout,Some(true)).unwrap().unwrap();
			assert!(utxo.confirmations>=6); assert_eq!(utxo.value,out.value); assert_eq!(utxo.script_pub_key.script().unwrap(),out.script_pubkey);
			assert!(point==payout||out.script_pubkey==operator_script||watch_points.contains(&point));
		}
		let final_pays=remote_pays(&lightning.internal,hash).await;
		assert_eq!(final_pays.len(),2);
		let failed=final_pays.iter().find(|pay|pay.label.as_deref()==Some(old_label.as_str())).unwrap();
		assert_eq!(failed.status(),ListpaysPaysStatus::Failed); assert_eq!(failed.created_index,failed_payment.created_index); assert!(failed.preimage.is_none());
		let paid=final_pays.iter().find(|pay|pay.label.as_deref()==Some(later_label.as_str())).unwrap();
		assert_eq!(paid.status(),ListpaysPaysStatus::Complete); assert_eq!(paid.preimage.as_deref(),Some(preimage.as_ref()));
		assert_eq!(paid.amount_msat.as_ref().unwrap().msat,100_000_000); assert_eq!(paid.amount_sent_msat.as_ref().unwrap().msat,100_000_000);
		let raw=lightning.internal.grpc_client().await.list_send_pays(cln_rpc::ListsendpaysRequest {
			payment_hash:Some(hash.to_vec()),..Default::default()
		}).await.unwrap().into_inner().payments;
		let mut own_paid=0;
		for part in &raw {
			assert_eq!(part.payment_hash,hash.to_vec());
			if part.label.as_deref()==Some(old_label.as_str()) {
				assert_eq!(part.status(),cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Failed);
				assert!(part.payment_preimage.is_none());
			} else {
				assert_eq!(part.label.as_deref(),Some(later_label.as_str()));
				assert_eq!(part.status(),cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Complete);
				assert_eq!(part.payment_preimage.as_deref(),Some(preimage.as_ref()));
				own_paid+=part.amount_msat.as_ref().unwrap().msat;
			}
		}
		assert_eq!(own_paid,100_000_000);
		let first_invoices=first_hold.list(hold::ListRequest { constraint:Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())) }).await.unwrap().into_inner().invoices;
		assert_eq!(first_invoices.len(),1); assert_eq!(first_invoices[0].state(),hold::InvoiceState::Cancelled);
		assert!(first_invoices[0].htlcs.iter().all(|htlc|htlc.state()!=hold::InvoiceState::Paid));
		let invoices=later_hold.list(hold::ListRequest { constraint:Some(hold::list_request::Constraint::PaymentHash(hash.to_vec())) }).await.unwrap().into_inner().invoices;
		assert_eq!(invoices.len(),1); assert_eq!(invoices[0].state(),hold::InvoiceState::Paid);
		assert_eq!(invoices[0].payment_hash,hash.to_vec()); assert_eq!(invoices[0].preimage.as_deref(),Some(preimage.as_ref()));
		assert_eq!(invoices[0].htlcs.iter().filter(|htlc|htlc.state()==hold::InvoiceState::Paid).map(|htlc|htlc.msat).sum::<u64>(),100_000_000);
		let attempts=db.read(async |tx|Ok(tx.query_one("SELECT COUNT(*) FROM lightning_payment_attempt WHERE payment_hash=$1",&[&hash.to_string()]).await?.get::<_,i64>(0))).await.unwrap();
		assert_eq!(attempts,2,"both source-distinct Ark attempts must remain historical");
		assert!(ctx.bitcoind().get_block_count().await-start_height<=96);
	})).await.expect("the two Ark attempts and returning previous store exceeded180seconds");
}
