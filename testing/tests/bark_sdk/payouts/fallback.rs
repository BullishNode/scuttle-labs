use ark_testing::TestContext;
use bitcoin::{Address, FeeRate, Network};
use bark::persist::{BarkPersister, sqlite::SqliteClient};
use ark_testing::sat;

#[tokio::test]
async fn fixed_mailbox_payout_is_discovered_and_spent_after_restart() {
	let ctx = TestContext::new("bark_sdk/fixed_mailbox_payout_spend_after_restart").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let mnemonic = bip39::Mnemonic::generate(12).unwrap();
	let wallet = ctx.bark_sdk("recipient", &srv).mnemonic(mnemonic.clone())
		.cfg(|c| c.daemon_manual_sync = true).create().await;
	wallet.stop_daemon_wait().await.unwrap();
	let script = wallet.fallback_script();
	let chain = wallet.chain().clone();
	ctx.bitcoind().fund_addr(Address::from_script(&script, Network::Regtest).unwrap(), sat(80_000)).await;
	ctx.generate_blocks(1).await;
	drop(wallet);

	let db = std::sync::Arc::new(SqliteClient::open(ctx.datadir.join("recipient/db.sqlite")).unwrap());
	let mut onchain = bark::onchain::OnchainWallet::load_or_create(
		Network::Regtest, mnemonic.to_seed(""), db).await.unwrap();
	onchain.sync(&chain).await.unwrap();
	assert_eq!(onchain.payout_balance().confirmed, sat(80_000));
	let payout_outpoint = onchain.payout_utxos()[0].outpoint;
	let destination = <bark::onchain::OnchainWallet as bark::onchain::OnchainWalletTrait>::address(
		&mut onchain).await.unwrap();
	let txid = onchain.send(&chain, destination, sat(70_000),
		FeeRate::from_sat_per_vb(2).unwrap()).await.unwrap();
	ctx.await_transaction(txid).await;
	ctx.generate_blocks(1).await;
	onchain.sync(&chain).await.unwrap();
	assert!(!onchain.payout_utxos().iter().any(|coin| coin.outpoint == payout_outpoint));
}

#[tokio::test]
async fn mailbox_identity_survives_seed_only_second_device_and_reopen() {
	let ctx = TestContext::new("bark_sdk/mailbox_identity_seed_only_reopen").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let mnemonic = bip39::Mnemonic::generate(12).unwrap();
	let first = ctx.bark_sdk("first", &srv).mnemonic(mnemonic.clone())
		.cfg(|c| c.daemon_manual_sync = true).create().await;
	first.stop_daemon_wait().await.unwrap();
	let original = first.fallback_script();
	let expected = {
		let onchain = first.onchain().unwrap();
		let guard = onchain.read().await;
		(&*guard as &dyn std::any::Any).downcast_ref::<bark::onchain::OnchainWallet>().unwrap().fallback_script()
	};
	assert_eq!(original, expected);
	let (address,index) = first.new_address_with_index().await.unwrap();
	assert_eq!(index, 0, "mailbox identity reserves no Ark coin child");
	let second = ctx.bark_sdk("second", &srv).mnemonic(mnemonic.clone()).create().await;
	second.stop_daemon_wait().await.unwrap();
	assert_eq!(second.fallback_script(), original);
	assert_eq!(second.new_address_with_index().await.unwrap().1, 0);
	assert_eq!(second.peek_keypair(0).await.unwrap().public_key(), address.policy().user_pubkey());
	let config = first.config().clone();
	drop(first);
	let reopened = bark::Wallet::open(Network::Regtest,
		bark::WalletSeed::new_from_mnemonic(Network::Regtest, &mnemonic), config,
		bark::OpenWalletArgs { datadir: Some(ctx.datadir.join("first")), run_daemon: false, ..Default::default() },
	).await.unwrap();
	assert_eq!(reopened.fallback_script(), original);
	assert_eq!(reopened.new_address_with_index().await.unwrap().1, 1);
}

/// An external mailbox controls both routing and the derived Taproot payout.
#[tokio::test]
async fn external_mailbox_identity_routes_delivery_and_refuses_wrong_reopen() {
	let ctx = TestContext::new("bark_sdk/external_mailbox_identity_routes_delivery").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let mnemonic = bip39::Mnemonic::generate(12).unwrap();
	let key = bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[41; 32]).unwrap();
	let first = ctx.bark_sdk("external", &srv).mnemonic(mnemonic.clone())
		.external_mailbox_keypair(key).cfg(|c| c.daemon_manual_sync = true).create().await;
	first.stop_daemon_wait().await.unwrap();
	assert_eq!(first.mailbox_identifier().as_pubkey(), key.public_key());
	let original = first.fallback_script();
	assert_eq!(original, bitcoin::ScriptBuf::new_p2tr(&ark::SECP, key.x_only_public_key().0, None));
	let (address, index) = first.new_address_with_index().await.unwrap();
	assert_eq!(index, 0);
	let [ark::address::VtxoDelivery::ServerMailbox { blinded_id }] = address.delivery() else {
		panic!("external mailbox address must use the upstream server mailbox format")
	};
	assert_eq!(ark::mailbox::MailboxIdentifier::from_blinded(
		*blinded_id, address.policy().user_pubkey(), &srv.mailbox_keypair()).unwrap(),
		first.mailbox_identifier());
	let onchain = first.onchain().unwrap();
	let guard = onchain.read().await;
	assert_eq!((&*guard as &dyn std::any::Any)
		.downcast_ref::<bark::onchain::OnchainWallet>().unwrap().fallback_script(), original);
	drop(guard);
	let properties = SqliteClient::open(ctx.datadir.join("external/db.sqlite")).unwrap()
		.read_properties().await.unwrap().unwrap();
	assert_eq!(properties.mailbox_identity, Some(bark::MailboxIdentity {
		network: Network::Regtest, public_key: key.public_key(),
	}));
	let db = std::sync::Arc::new(SqliteClient::open(ctx.datadir.join("external/db.sqlite")).unwrap());
	assert!(bark::onchain::OnchainWallet::load_or_create(
		Network::Regtest, mnemonic.to_seed(""), db).await.is_err(),
		"a seed-derived payout descriptor must not open an external-mailbox watch");
	let config = first.config().clone();
	let sender = ctx.bark_sdk("sender", &srv).boarded(ark_testing::sat(100_000)).create().await;
	sender.send_arkoor_payment(&address, ark_testing::sat(10_000)).await.unwrap();
	first.sync().await;
	assert_eq!(first.balance().await.unwrap().spendable, ark_testing::sat(10_000));
	drop(first);
	let seed = || bark::WalletSeed::new_from_mnemonic(Network::Regtest, &mnemonic);
	let directory = ctx.datadir.join("external");
	let wrong = bitcoin::secp256k1::Keypair::from_secret_key(&ark::SECP, &key.secret_key().negate());
	for supplied in [seed(), seed().with_external_mailbox_keypair(wrong)] {
		let result = bark::Wallet::open(Network::Regtest, supplied, config.clone(), bark::OpenWalletArgs {
			datadir: Some(directory.clone()), run_daemon: false, ..Default::default()
		}).await;
		let error = match result { Ok(_) => panic!("wrong or missing mailbox key must refuse reopen"), Err(error) => error };
		assert!(error.to_string().contains("mailbox key"));
	}
	let without_watch = bark::Wallet::open(Network::Regtest,
		seed().with_external_mailbox_keypair(key), config.clone(), bark::OpenWalletArgs {
			datadir: Some(directory.clone()), run_daemon: false, ..Default::default()
		}).await;
	let error = match without_watch {
		Ok(_) => panic!("external mailbox must not open without a payout watch"),
		Err(error) => error,
	};
	assert!(error.to_string().contains("external mailbox requires an onchain wallet"));
	let reopened = bark::Wallet::open(Network::Regtest, seed().with_external_mailbox_keypair(key), config,
		bark::OpenWalletArgs { datadir: Some(directory), onchain: Some(onchain),
			run_daemon: false, ..Default::default() },
	).await.unwrap();
	assert_eq!(reopened.mailbox_identifier().as_pubkey(), key.public_key());
	assert_eq!(reopened.fallback_script(), original);
	assert_eq!(reopened.balance().await.unwrap().spendable, ark_testing::sat(10_000));
}
