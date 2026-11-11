use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::{Invoice, Preimage};
use ark::VtxoPolicy;
use bitcoin_ext::{BlockDelta, BlockHeight};
use server::database::ln::LightningPaymentStatus;
use server_rpc::protos;
use cln_rpc::plugins::hold;

use ark_testing::{btc, sat, TestContext};
use ark_testing::util::poll_interval;

#[tokio::test]
async fn lightning_rejection_refunds_only_its_source() {
	tokio::time::timeout(Duration::from_secs(180), async {
		let ctx = TestContext::new("server/lightning_rejection_refunds_only_its_source").await;
		let lightning = ctx.new_lightning_setup("lightningd").await;
		let srv = ctx.captaind("server").lightningd(&lightning.internal).no_vtxo_pool().funded(btc(10))
			.cfg(|cfg| { cfg.expiry_payouts = true; cfg.invoice_check_interval = Duration::from_secs(1); }).watchmand().create().await;
		let sender = ctx.bark("sender", &srv).funded(sat(500_000)).create().await;
		let boards = [sender.board_and_confirm_and_register(&ctx, sat(100_000)).await[0],
			sender.board_and_confirm_and_register(&ctx, sat(100_000)).await[0]];
		let wallet = sender.client().await;
		let hash = Preimage::from_slice(&[73;32]).unwrap().compute_payment_hash();
		let principal = sat(90_000);
		// Delete a genuine, still-unpaid receiver invoice before any send. A
		// normally routed HTLC must then fail at the receiver, producing actual
		// failed native parts rather than treating an arbitrary RPC error as cash authority.
		let invoice = lightning.external.invoice_with_preimage(Some(principal),
			"deleted-before-dispatch", "native receiver rejection", [73; 32]).await;
		let mut receiver = lightning.external.grpc_client().await;
		let deleted = receiver.del_invoice(cln_rpc::DelinvoiceRequest {
			label: "deleted-before-dispatch".into(),
			status: cln_rpc::delinvoice_request::DelinvoiceStatus::Unpaid as i32, desconly: None,
		}).await.unwrap().into_inner();
		assert_eq!(deleted.status(), cln_rpc::delinvoice_response::DelinvoiceStatus::Unpaid);
		assert_eq!(deleted.payment_hash, hash.to_vec());
		assert_eq!(deleted.bolt11.as_deref(), Some(invoice.as_str()));
		assert!(deleted.payment_preimage.is_none());
		assert!(receiver.list_invoices(cln_rpc::ListinvoicesRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().invoices.is_empty());
		let invoice = invoice.parse::<Invoice>().unwrap();
		assert_eq!(invoice.payment_hash(), hash);
		let tip = BlockHeight::new(ctx.bitcoind().get_block_count().await as u32);
		let deadline = tip + srv.config().htlc_send_expiry_delta + BlockDelta::new(2);
		let mut rpc = srv.get_public_rpc().await;
		let mut sources = Vec::new();
		for id in boards {
			let input = wallet.get_full_vtxo(id).await.unwrap();
			let key = wallet.get_vtxo_key(&input).await.unwrap();
			let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input],
				VtxoPolicy::new_server_htlc_send(key.public_key(), hash, deadline)).unwrap()
				.generate_user_nonces(&[key]).unwrap();
			let response = rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
				parts: super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
			}).await.unwrap().into_inner();
			let mut grants = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
				.unwrap().build_signed_vtxos();
			assert_eq!(grants.len(), 1);
			assert_eq!(grants[0].amount(), sat(100_000));
			wallet.register_vtxo_transactions_with_server(&grants).await.unwrap();
			sources.push((grants.remove(0), key));
		}
		lightning.sync().await;
		let mut payer = lightning.internal.grpc_client().await;
		let info = payer.getinfo(cln_rpc::GetinfoRequest {}).await.unwrap().into_inner();
		assert_eq!(info.network, "regtest");
		let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
		let node = db.read(async |t| Ok(t.query_one("SELECT id,pubkey FROM lightning_node WHERE pubkey=$1",
			&[&info.id]).await?)).await.unwrap();
		assert_eq!(node.get::<_, Vec<u8>>("pubkey"), info.id);
		let node_id = node.get::<_, i64>("id");
		assert!(payer.list_send_pays(cln_rpc::ListsendpaysRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().payments.is_empty());
		assert!(payer.list_pays(cln_rpc::ListpaysRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().pays.is_empty());
		let mut returned = srv.subscribe_log::<server_log::XpayRpcReturned>();
		rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
			protos::InitiateLightningPaymentRequest {
			invoice: invoice.to_string(), payment_amount_sat: principal.to_sat(),
			htlc_vtxo_ids: vec![sources[0].0.id().to_bytes().to_vec()], mailbox_id: None, retry_for_secs: Some(1),
			intent_nonce: None, source_owner_signatures: Vec::new(),
		}, &sources[0].1).await).await.unwrap();
		tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				let result = returned.recv().await.unwrap();
				if result.payment_hash == hash {
					assert!(result.error.is_some());
					break;
				}
			}
		}).await.expect("the product dispatch must reach the native receiver rejection");
		let rejected = tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				if let Some(attempt) = db.read(async |t| t.get_latest_payment_attempt_by_payment_hash(hash).await).await.unwrap() {
					if attempt.status == LightningPaymentStatus::Failed { break attempt; }
				}
				tokio::time::sleep(poll_interval()).await;
			}
		}).await.expect("the native receiver rejection must close its own attempt");
		assert_eq!(rejected.lightning_node_id, node_id);
		assert_eq!(rejected.amount_msat, principal.to_sat() * 1_000);
		assert_eq!(rejected.final_amount_msat, Some(0));
		let links = db.read(async |t| Ok(t.query("SELECT vtxo_id FROM lightning_payment_attempt_htlc_vtxo \
			WHERE lightning_payment_attempt_id=$1 ORDER BY vtxo_id", &[&rejected.id]).await?
			.into_iter().map(|row| row.get::<_, String>(0)).collect::<Vec<_>>())).await.unwrap();
		assert_eq!(links, vec![sources[0].0.id().to_string()]);
		assert!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
		let rejected_label = ark_testing::lightning::attempt_xpay_label(&db, rejected.id).await;
		let rejected_parts = payer.list_send_pays(cln_rpc::ListsendpaysRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().payments;
		assert!(!rejected_parts.is_empty());
		assert!(rejected_parts.iter().all(|part| part.payment_hash == hash.to_vec()
			&& part.label.as_deref() == Some(rejected_label.as_str())
			&& part.groupid == rejected_parts[0].groupid && part.created_index > 0
			&& part.status() == cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Failed
			&& part.payment_preimage.is_none()));
		assert_eq!(rejected_parts.iter().map(|part| part.amount_msat.as_ref().unwrap().msat).sum::<u64>(), principal.to_sat() * 1_000);
		let rejected_pays = payer.list_pays(cln_rpc::ListpaysRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().pays;
		assert_eq!(rejected_pays.len(), 1);
		assert_eq!(rejected_pays[0].label.as_deref(), Some(rejected_label.as_str()));
		assert_eq!(rejected_pays[0].payment_hash, hash.to_vec());
		assert_eq!(rejected_pays[0].status(), cln_rpc::listpays_pays::ListpaysPaysStatus::Failed);
		assert!(rejected_pays[0].created_index.unwrap() > 0);
		assert!(rejected_pays[0].preimage.is_none());

		// Only after the definite rejection, create the same-hash held sibling.
		let invoice = lightning.external.hold_client().await.invoice(hold::InvoiceRequest {
			payment_hash: hash.to_vec(), amount_msat: principal.to_sat() * 1_000,
			description: Some(hold::invoice_request::Description::Memo("same-hash pending source".into())),
			min_final_cltv_expiry: Some(18), expiry: Some(3600), routing_hints: vec![],
		}).await.unwrap().into_inner().bolt11;
		rpc.initiate_lightning_payment(ark_testing::lightning::sign_lightning_send_request(&srv,
			protos::InitiateLightningPaymentRequest {
			invoice, payment_amount_sat: principal.to_sat(),
			htlc_vtxo_ids: vec![sources[1].0.id().to_bytes().to_vec()], mailbox_id: None, retry_for_secs: Some(1),
			intent_nonce: None, source_owner_signatures: Vec::new(),
		}, &sources[1].1).await).await.unwrap();
		lightning.external.wait_for_hold_invoice_accepted(hash).await;
		let pending = db.read(async |t| t.get_open_lightning_payment_attempt_by_payment_hash(hash).await).await.unwrap().unwrap();
		assert_ne!(pending.id, rejected.id);
		assert!(matches!(pending.status, LightningPaymentStatus::Requested | LightningPaymentStatus::Submitted));
		let label = ark_testing::lightning::attempt_xpay_label(&db, pending.id).await;
		let native_parts = payer.list_send_pays(cln_rpc::ListsendpaysRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().payments;
		assert_eq!(native_parts.iter().filter(|part| part.label.as_deref() == Some(rejected_label.as_str()))
			.cloned().collect::<Vec<_>>(), rejected_parts);
		let parts = native_parts.iter().filter(|part| part.label.as_deref() == Some(label.as_str()))
			.cloned().collect::<Vec<_>>();
		assert_eq!(native_parts.len(), rejected_parts.len() + parts.len());
		assert!(!parts.is_empty());
		assert!(parts.iter().all(|part| part.label.as_deref() == Some(label.as_str()) && part.payment_hash == hash.to_vec()
			&& part.status() == cln_rpc::listsendpays_payments::ListsendpaysPaymentsStatus::Pending && part.payment_preimage.is_none()));
		assert_eq!(parts.iter().map(|part| part.amount_msat.as_ref().unwrap().msat).sum::<u64>(), principal.to_sat() * 1_000);
		let native_pays = payer.list_pays(cln_rpc::ListpaysRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().pays;
		// Native xpay overlays Pending on old failed groups while this same hash is active.
		let mut expected_rejected_pays = rejected_pays.clone();
		expected_rejected_pays[0].status = cln_rpc::listpays_pays::ListpaysPaysStatus::Pending as i32;
		assert_eq!(native_pays.iter().filter(|pay| pay.label.as_deref() == Some(rejected_label.as_str()))
			.cloned().collect::<Vec<_>>(), expected_rejected_pays);
		let aggregate = native_pays.iter().filter(|pay| pay.label.as_deref() == Some(label.as_str())).collect::<Vec<_>>();
		assert_eq!(native_pays.len(), rejected_pays.len() + aggregate.len());
		assert_eq!(aggregate.len(), 1);
		assert_eq!(aggregate[0].label.as_deref(), Some(label.as_str()));
		assert_eq!(aggregate[0].status(), cln_rpc::listpays_pays::ListpaysPaysStatus::Pending);
		assert!(aggregate[0].preimage.is_none());
		for (index, (source, _key)) in sources.iter().enumerate() {
			let status = rpc.check_lightning_payment(protos::CheckLightningPaymentRequest {
				hash: hash.to_vec(), wait: false, htlc_vtxo_ids: vec![source.id().to_bytes().to_vec()],
			}).await.unwrap().into_inner();
			assert_eq!(status.htlc_vtxo_ids, vec![source.id().to_bytes().to_vec()]);
			if index == 0 { assert!(matches!(status.payment_status, Some(protos::lightning_payment_status::PaymentStatus::Failed(_)))); }
			else { assert!(matches!(status.payment_status, Some(protos::lightning_payment_status::PaymentStatus::Pending(_)))); }
		}
		for (index, (source, key)) in sources.iter().enumerate() {
			let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([source.clone()],
				VtxoPolicy::new_pubkey(key.public_key())).unwrap().generate_user_nonces(&[*key]).unwrap();
			let response = rpc.request_lightning_pay_htlc_revocation(
				super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await,
			).await;
			if index == 0 {
				let internal = builder.build_unsigned_internal_vtxos();
				assert_eq!(internal.len(), 1);
				let txids = builder.virtual_transactions().collect::<Vec<_>>();
				let refund = builder.user_cosign(&[*key], ArkoorPackageCosignResponse::try_from(response.unwrap().into_inner()).unwrap())
					.unwrap().build_signed_vtxos();
				assert_eq!(refund.iter().map(|coin| coin.amount()).sum::<bitcoin::Amount>(), sat(100_000));
				wallet.register_vtxo_transactions_with_server(&refund).await.unwrap();
				let resolved = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, source.id()).await).await.unwrap().unwrap();
				assert_eq!(resolved.htlc.offchain_resolution, Some(server::database::htlc_vtxo::HtlcResolution::Revoked));
				// A different destination requests a second spend. Replaying the
				// identical signed transaction is an ordinary idempotent retry.
				let other_destination = sources[1].1.public_key();
				assert_ne!(other_destination, key.public_key());
				let repeat = ArkoorPackageBuilder::new_claim_all_with_checkpoints([source.clone()],
					VtxoPolicy::new_pubkey(other_destination)).unwrap().generate_user_nonces(&[*key]).unwrap();
				let repeated_internal = repeat.build_unsigned_internal_vtxos();
				assert_eq!(repeated_internal.len(), 1);
				assert_eq!(repeated_internal[0].0.id(), internal[0].0.id());
				assert_ne!(repeated_internal[0].1, internal[0].1);
				let repeated_outputs = repeat.build_unsigned_vtxos().collect::<Vec<_>>();
				assert_eq!(repeated_outputs.len(), 1);
				assert_ne!(repeated_outputs[0].id(), refund[0].id());
				let new_txids = repeat.virtual_transactions().filter(|txid| !txids.contains(txid)).collect::<Vec<_>>();
				assert_eq!(new_txids.len(), 1);
				for txid in &new_txids {
					assert!(db.read(async |t| t.get_virtual_transaction_by_txid(*txid).await).await.unwrap().is_none());
				}
				assert!(db.read(async |t| Ok(t.query_opt("SELECT vtxo_id FROM vtxo WHERE vtxo_id=$1",
					&[&repeated_outputs[0].id().to_string()]).await?.is_none())).await.unwrap());
				let affected_coins = [source.id(), internal[0].0.id(), refund[0].id(), repeated_outputs[0].id()]
					.map(|id| id.to_string()).to_vec();
				let affected_txs = txids.iter().chain(new_txids.iter()).map(ToString::to_string).collect::<Vec<_>>();
				let read_state = async || db.read(async |t| {
					let row = t.query_one("SELECT \
						(SELECT COALESCE(jsonb_agg(to_jsonb(v) ORDER BY v.vtxo_id),'[]'::jsonb)::TEXT FROM vtxo v WHERE v.vtxo_id=ANY($1)) AS coins, \
						(SELECT COALESCE(jsonb_agg(to_jsonb(h) ORDER BY h.id),'[]'::jsonb)::TEXT FROM htlc_vtxo h JOIN vtxo v ON v.id=h.id WHERE v.vtxo_id=ANY($1)) AS htlcs, \
						(SELECT COALESCE(jsonb_agg(to_jsonb(v) ORDER BY v.txid),'[]'::jsonb)::TEXT FROM virtual_transaction v WHERE v.txid=ANY($2)) AS txs, \
						(SELECT COUNT(*) FROM virtual_transaction_history WHERE txid=ANY($2)) AS tx_history",
						&[&affected_coins, &affected_txs]).await?;
					Ok((row.get::<_, String>("coins"), row.get::<_, String>("htlcs"),
						row.get::<_, String>("txs"), row.get::<_, i64>("tx_history")))
				}).await.unwrap();
				let before = read_state().await;
				assert!(srv.config().rpc_rich_errors);
				let refusal = rpc.request_lightning_pay_htlc_revocation(super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(repeat.cosign_request())).await)
					.await.unwrap_err();
				assert_eq!(refusal.code(), tonic::Code::Internal);
				assert_eq!(refusal.message(), format!("db tx body error: vtxo {} is already spent", internal[0].0.id()));
				assert_eq!(read_state().await, before);
			} else { assert_eq!(response.unwrap_err().code(), tonic::Code::InvalidArgument); }
		}
		let after = db.read(async |t| t.get_open_lightning_payment_attempt_by_payment_hash(hash).await).await.unwrap().unwrap();
		assert_eq!(after.id, pending.id);
		assert!(matches!(after.status, LightningPaymentStatus::Requested | LightningPaymentStatus::Submitted));
		let held = db.read(async |t| server::database::htlc_vtxo::get_htlc_vtxo(t, sources[1].0.id()).await).await.unwrap().unwrap();
		assert!(held.htlc.offchain_resolution.is_none());
		let state = db.read(async |t| Ok(t.query_one("SELECT v.spend_state::TEXT, \
			EXISTS(SELECT 1 FROM expiry_entitlement WHERE vtxo_id=ANY($2)) AS entitled \
			FROM vtxo v WHERE v.vtxo_id=$1", &[&sources[1].0.id().to_string(),
				&sources.iter().map(|(source,_)| source.id().to_string()).collect::<Vec<_>>()]).await?)).await.unwrap();
		assert_eq!(state.get::<_, &str>("spend_state"), "spendable");
		assert!(!state.get::<_, bool>("entitled"));
		assert_eq!(payer.list_send_pays(cln_rpc::ListsendpaysRequest {
			payment_hash: Some(hash.to_vec()), ..Default::default()
		}).await.unwrap().into_inner().payments, native_parts);
		assert!(db.read(async |t| t.get_htlc_settlement_by_payment_hash(hash).await).await.unwrap().is_none());
	}).await.expect("typed rejection/source isolation exceeded the unchanged 180-second limit");
}
