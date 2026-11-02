use ark_testing::{sat, TestContext};
use bitcoin::{Amount, OutPoint, Transaction};
use bitcoin_ext::TxOutExt;
use bitcoin_ext::rpc::{BitcoinRpcExt, RpcApi, json::SignRawTransactionInput};

// The original refund may already have a funded mempool child. Replace it
// through ordinary package relay using the holder's real signed prefix.
pub(super) async fn replace_pending_refund(ctx: &TestContext, prefix: &Transaction, refund: &Transaction) {
	#[derive(serde::Deserialize)]
	struct Fees { #[serde(with = "bitcoin::amount::serde::as_btc")] descendant: Amount }
	#[derive(serde::Deserialize)]
	struct Entry { fees: Fees }
	let chain = ctx.bitcoind().sync_client();
	let old_fee = if chain.get_mempool_entry(&refund.compute_txid()).is_ok() {
		chain.call::<Entry>("getmempoolentry", &[serde_json::json!(refund.compute_txid())]).unwrap().fees.descendant
	} else { Amount::ZERO };
	let fee = old_fee + sat(20_000);
	let anchor_index = prefix.output.iter().position(|output| output.is_p2a_fee_anchor()).unwrap();
	let anchor = OutPoint::new(prefix.compute_txid(), anchor_index as u32);
	let coin = chain.list_unspent(Some(1), None, None, None, None).unwrap().into_iter()
		.find(|coin| coin.spendable && coin.solvable && coin.safe && coin.amount > fee + sat(1_000)).unwrap();
	let child = Transaction {
		version: bitcoin::transaction::Version::non_standard(3), lock_time: bitcoin::absolute::LockTime::ZERO,
		input: [anchor, OutPoint::new(coin.txid, coin.vout)].into_iter().map(|point| bitcoin::TxIn {
			previous_output: point, script_sig: Default::default(), sequence: bitcoin::Sequence::ZERO,
			witness: bitcoin::Witness::new(),
		}).collect(),
		output: vec![bitcoin::TxOut { value: coin.amount + prefix.output[anchor_index].value - fee,
			script_pubkey: ctx.bitcoind().get_new_address().script_pubkey() }],
	};
	let prevout = SignRawTransactionInput { txid: anchor.txid, vout: anchor.vout,
		script_pub_key: prefix.output[anchor_index].script_pubkey.clone(), redeem_script: None,
		amount: Some(prefix.output[anchor_index].value) };
	let child = chain.sign_raw_transaction_with_wallet(&child, Some(&[prevout]), None).unwrap().transaction().unwrap();
	let result = chain.submit_package(&[prefix, &child]).unwrap();
	assert_eq!(result.package_msg, "success");
	for tx in [prefix, &child] {
		assert!(result.tx_results.values().any(|entry| entry.txid == tx.compute_txid() && entry.error.is_none()));
		assert_eq!(ctx.bitcoind().await_transaction(tx.compute_txid()).await, *tx);
	}
	assert!(chain.get_mempool_entry(&refund.compute_txid()).is_err());
}
