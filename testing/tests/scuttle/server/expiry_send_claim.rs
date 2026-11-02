//! A shared preimage does not discharge an unrelated send source.

use std::sync::Arc;
use std::time::Duration;

use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::PaymentHash;
use ark::vtxo::Full;
use ark::{Vtxo, VtxoPolicy};
use ark_testing::{btc, TestContext};
use ark_testing::daemon::captaind::Captaind;
use ark_testing::daemon::lightningd::Lightningd;
use bitcoin_ext::BlockDelta;
use bitcoin_ext::rpc::RpcApi;
use server_rpc::protos;


pub(super) async fn claim_server(ctx: &TestContext, node: &Lightningd) -> Arc<Captaind> {
	let drain = ctx.bitcoind().get_new_address();
	ctx.captaind("server").lightningd(node).bitcoind(ctx.bitcoind_arc()).no_vtxo_pool().funded(btc(10))
		.cfg(|cfg| {
			cfg.vtxo_lifetime = BlockDelta::new(1_000);
			cfg.vtxopool.vtxo_lifetime = BlockDelta::new(1_000);
			cfg.vtxo_exit_delta = BlockDelta::new(144);
			cfg.htlc_expiry_delta = BlockDelta::new(40);
		}).watchmand().watchmand_cfg(move |cfg| {
			cfg.sweep_address = Some(drain.into_unchecked());
			cfg.watchman.reaction_interval = Duration::from_secs(600);
			cfg.watchman.sweep_interval = Duration::from_secs(600);
		}).create().await
}

pub(super) async fn signed_leaf(
	ctx: &TestContext, srv: &Captaind, wallet: &bark::Wallet,
	input: Vtxo<Full>, policy: VtxoPolicy,
) -> Vtxo<Full> {
	let key = wallet.get_vtxo_key(&input).await.unwrap();
	let builder = ArkoorPackageBuilder::new_claim_all_with_checkpoints([input.clone()], policy.clone()).unwrap()
		.generate_user_nonces(&[key]).unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let response = if matches!(&policy, VtxoPolicy::ServerHtlcSend(_)) {
		rpc.request_lightning_pay_htlc_cosign(protos::LightningPayHtlcCosignRequest {
			parts: super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await.parts,
		}).await.unwrap().into_inner()
	} else {
		rpc.request_arkoor_cosign(super::signed_package(&srv, &[&wallet], protos::ArkoorPackageCosignRequest::from(builder.cosign_request())).await)
			.await.unwrap().into_inner()
	};
	let mut outputs = builder.user_cosign(&[key], ArkoorPackageCosignResponse::try_from(response).unwrap())
		.unwrap().build_signed_vtxos();
	assert_eq!(outputs.len(), 1);
	let leaf = outputs.remove(0);
	assert_eq!(leaf.policy(), &policy);
	assert_eq!(leaf.amount(), input.amount());
	assert_eq!(leaf.chain_anchor(), input.chain_anchor());
	assert_eq!(leaf.exit_delta(), input.exit_delta());
	assert_eq!(leaf.exit_delta(), srv.config().vtxo_exit_delta);
	let funding = ctx.bitcoind().await_transaction(leaf.chain_anchor().txid).await;
	leaf.validate(&funding).unwrap();
	wallet.register_vtxo_transactions_with_server(&[leaf.clone()]).await.unwrap();
	leaf
}

pub(super) fn confirmed_height(ctx: &TestContext, leaf: &Vtxo<Full>) -> u32 {
	let chain = ctx.bitcoind().sync_client();
	let output = chain.get_tx_out(&leaf.point().txid, leaf.point().vout, Some(false)).unwrap().unwrap();
	assert!(output.confirmations > 0);
	assert_eq!(output.value, leaf.amount());
	assert_eq!(output.script_pub_key.script().unwrap(), leaf.txout().script_pubkey);
	u32::try_from(chain.get_block_count().unwrap() + 1 - u64::from(output.confirmations)).unwrap()
}

pub(super) fn assert_unspent(ctx: &TestContext, leaf: &Vtxo<Full>) {
	let chain = ctx.bitcoind().sync_client();
	for include_mempool in [false, true] {
		let output = chain.get_tx_out(&leaf.point().txid, leaf.point().vout, Some(include_mempool)).unwrap().unwrap();
		assert!(output.confirmations > 0);
		assert_eq!(output.value, leaf.amount());
		assert_eq!(output.script_pub_key.script().unwrap(), leaf.txout().script_pubkey);
	}
	for txid in chain.get_raw_mempool().unwrap() {
		let tx = chain.get_raw_transaction(&txid, None).unwrap();
		assert!(tx.input.iter().all(|input| input.previous_output != leaf.point()),
			"no mempool transaction may consume the unpaid source");
	}
}

pub(super) async fn send_history(node: &Lightningd, hash: PaymentHash) -> (Vec<cln_rpc::ListsendpaysPayments>, Vec<cln_rpc::ListpaysPays>) {
	let mut rpc = node.grpc_client().await;
	let mut parts = rpc.list_send_pays(cln_rpc::ListsendpaysRequest::default()).await.unwrap().into_inner()
		.payments.into_iter().filter(|part| part.payment_hash == hash.to_vec()).collect::<Vec<_>>();
	parts.sort_by_key(|part| (part.groupid, part.partid, part.id));
	let mut pays = rpc.list_pays(cln_rpc::ListpaysRequest::default()).await.unwrap().into_inner()
		.pays.into_iter().filter(|pay| pay.payment_hash == hash.to_vec()).collect::<Vec<_>>();
	pays.sort_by_key(|pay| pay.created_index);
	(parts, pays)
}

