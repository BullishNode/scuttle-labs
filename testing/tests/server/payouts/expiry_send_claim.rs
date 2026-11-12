//! A shared preimage does not discharge an unrelated send source.


use ark::arkoor::package::{ArkoorPackageBuilder, ArkoorPackageCosignResponse};
use ark::lightning::PaymentHash;
use ark::vtxo::Full;
use ark::{Vtxo, VtxoPolicy};
use ark_testing::TestContext;
use ark_testing::daemon::captaind::Captaind;
use ark_testing::daemon::lightningd::Lightningd;
use server_rpc::protos;

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
