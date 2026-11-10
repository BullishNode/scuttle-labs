use ark_testing::TestContext;
use bark::{Config, OpenWalletArgs, Wallet, WalletSeed};
use bitcoin::Network;

#[tokio::test]
async fn required_scuttle_label_refuses_offline_creation_before_store() {
	let ctx = TestContext::new("bark_sdk/required_scuttle_offline").await;
	let datadir = ctx.datadir.join("no-wallet-store");
	let mut config = Config::network_default(Network::Regtest);
	config.server_address = "http://127.0.0.1:1".to_owned();
	config.require_scuttle_payouts = true;
	let error = Wallet::open(Network::Regtest,
		WalletSeed::new_from_seed(Network::Regtest, &[42; 64]), config,
		OpenWalletArgs {
			datadir: Some(datadir.clone()),
			create_without_server: true,
			run_daemon: false,
			..Default::default()
		},
	).await.err().expect("offline creation must fail when payouts are required");
	assert!(format!("{error:#}").contains("Failed to connect to Ark server"), "{error:#}");
	assert!(!datadir.exists(), "required label must be checked before creating the wallet store");
}

#[tokio::test]
#[ignore = "requires SCUTTLE_CONSUMER_EXE built from a pinned Scuttle Git revision"]
async fn external_git_consumer_opens_against_paired_payout_server() {
	let consumer = std::env::var_os("SCUTTLE_CONSUMER_EXE")
		.expect("SCUTTLE_CONSUMER_EXE must name the pinned external consumer binary");
	let ctx = TestContext::new("bark_sdk/external_git_consumer").await;
	let payout = ctx.captaind("payout").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let output = tokio::process::Command::new(consumer)
		.env("SCUTTLE_ARK_URL", payout.ark_url())
		.env("SCUTTLE_BITCOIND_URL", ctx.bitcoind().rpc_url())
		.env("SCUTTLE_BITCOIND_COOKIE", ctx.bitcoind().rpc_cookie())
		.env("SCUTTLE_WALLET_DIR", ctx.datadir.join("external-consumer"))
		.output().await.expect("run external consumer");
	assert!(output.status.success(), "consumer exit: {}\nstdout: {}\nstderr: {}",
		output.status,
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr),
	);
	assert!(String::from_utf8_lossy(&output.stdout).contains("scuttle-1 wallet open"));
}

#[tokio::test]
async fn required_scuttle_label_is_checked_when_the_wallet_opens() {
	let ctx = TestContext::new("bark_sdk/required_scuttle_label").await;
	let payout = ctx.captaind("payout").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let required = ctx.bark_sdk("required", &payout)
		.cfg(|cfg| cfg.require_scuttle_payouts = true).create().await;
	assert_eq!(required.properties().await.unwrap().server_payouts, Some(true));
	required.refresh_server().await.unwrap();

	let ordinary = ctx.captaind("ordinary").create().await;
	let error = ctx.bark_sdk("rejected", &ordinary)
		.cfg(|cfg| cfg.require_scuttle_payouts = true).try_create().await
		.err().expect("a required payout wallet must reject an ordinary server");
	assert!(format!("{error:#}").contains("protocol label mismatch"), "{error:#}");

	let upstream = ctx.bark_sdk("upstream", &ordinary).create().await;
	assert_eq!(upstream.properties().await.unwrap().server_payouts, Some(false));

	let error = ctx.bark_sdk("unreachable", &ordinary)
		.cfg(|cfg| {
			cfg.server_address = "http://127.0.0.1:1".to_owned();
			cfg.require_scuttle_payouts = true;
		}).try_create().await.err().expect("a required payout wallet must reach its server");
	assert!(format!("{error:#}").contains("Failed to connect to Ark server"), "{error:#}");
}
