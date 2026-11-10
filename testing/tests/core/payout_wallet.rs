use ark_testing::{sat, TestContext};
use ark_testing::payout_wallet::PayoutWallet;
use bitcoin::{Address, FeeRate, Network, OutPoint};
use bitcoin::bip32::{DerivationPath, Xpriv};
use bitcoin_ext::rpc::RpcApi;

#[tokio::test]
async fn independent_mailbox_payout_wallet_restores_and_spends_both_key_parities() {
	let ctx = TestContext::new("core/independent_mailbox_payout_wallet").await;
	ctx.bitcoind().prepare_funds().await;
	let mut seeds = [None, None];
	let path = "m/350'/1'".parse::<DerivationPath>().unwrap();
	for byte in 1..=32 {
		let seed = [byte; 64];
		let key = Xpriv::new_master(Network::Regtest, &seed).unwrap()
			.derive_priv(&ark::SECP, &path).unwrap().to_keypair(&ark::SECP);
		let parity = usize::from(key.public_key().serialize()[0] - 2);
		if seeds[parity].is_none() { seeds[parity] = Some(seed); }
		if seeds.iter().all(Option::is_some) { break; }
	}
	assert!(seeds.iter().all(Option::is_some), "need even and odd full mailbox keys");
	for (parity, seed) in seeds.into_iter().enumerate() {
		let seed = seed.unwrap();
		let script = PayoutWallet::script_from_seed(&seed, Network::Regtest).unwrap();
		let address = Address::from_script(&script, Network::Regtest).unwrap();
		let funding_txid = ctx.bitcoind().fund_addr(address, sat(80_000)).await;
		ctx.generate_blocks(1).await;
		let funding = ctx.bitcoind().await_transaction(funding_txid).await;
		let vout = funding.output.iter().position(|output|
			output.script_pubkey == script && output.value == sat(80_000)).unwrap();
		let payout = OutPoint::new(funding_txid, vout as u32);
		let dir = ctx.datadir.join(format!("payout-wallet-{parity}"));
		let mut restored = PayoutWallet::restore(&seed, Network::Regtest, &dir).unwrap();
		assert_eq!(restored.script(), &script);
		restored.rescan(ctx.bitcoind()).unwrap();
		restored.confirmed_payout(payout, sat(80_000)).unwrap();
		let destination = ctx.bitcoind().get_new_address();
		let spend = restored.spend(ctx.bitcoind(), payout, sat(80_000), &destination,
			FeeRate::from_sat_per_vb(2).unwrap(), &dir).unwrap();
		ctx.generate_blocks(1).await;
		assert!(ctx.bitcoind().sync_client().get_tx_out(&spend.output.txid,
			spend.output.vout, Some(true)).unwrap().unwrap().confirmations >= 1);
		assert!(ctx.bitcoind().sync_client().get_tx_out(&payout.txid,
			payout.vout, Some(true)).unwrap().is_none());
		let report: serde_json::Value = serde_json::from_slice(
			&std::fs::read(dir.join("spend.json")).unwrap()).unwrap();
		assert_eq!(report["payout_outpoint"], payout.to_string());
		assert_eq!(report["spend_txid"], spend.txid.to_string());
		assert_eq!(report["spend_fee_sat"], spend.fee.to_sat());
	}
}
