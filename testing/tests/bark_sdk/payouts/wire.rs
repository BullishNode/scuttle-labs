use ark_testing::TestContext;
use ark_testing::sat;

#[tokio::test]
#[ignore = "requires SCUTTLE_CONSUMER_EXE built from a pinned Scuttle Git revision"]
async fn external_git_consumer_sends_and_receives_with_paired_payout_server() {
	let consumer = std::env::var_os("SCUTTLE_CONSUMER_EXE")
		.expect("SCUTTLE_CONSUMER_EXE must name the pinned external consumer binary");
	let ctx = TestContext::new("bark_sdk/external_git_consumer").await;
	let payout = ctx.captaind("payout").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let run = |action: &'static str, destination: Option<String>| {
		let mut command = tokio::process::Command::new(&consumer);
		command.env("SCUTTLE_ARK_URL", payout.ark_url())
			.env("SCUTTLE_BITCOIND_URL", ctx.bitcoind().rpc_url())
			.env("SCUTTLE_BITCOIND_COOKIE", ctx.bitcoind().rpc_cookie())
			.env("SCUTTLE_WALLET_DIR", ctx.datadir.join("external-consumer"))
			.env("SCUTTLE_ACTION", action);
		if let Some(address) = destination {
			command.env("SCUTTLE_SEND_ADDRESS", address)
				.env("SCUTTLE_SEND_SATS", "50000");
		}
		command
	};
	let output = run("address", None).output().await.expect("run external consumer");
	assert!(output.status.success(), "consumer exit: {}\nstdout: {}\nstderr: {}",
		output.status,
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr),
	);
	let address_line = String::from_utf8_lossy(&output.stdout);
	let address = address_line.trim().strip_prefix("scuttle-1 wallet open: ")
		.expect("external consumer produced an Ark address").parse().unwrap();
	let sender = ctx.bark_sdk("sender", &payout).boarded(sat(400_000)).create().await;
	sender.send_arkoor_payment(&address, sat(100_000)).await.unwrap();
	let received = run("sync", None).output().await.expect("sync external consumer");
	assert!(received.status.success(), "external receive: {}", String::from_utf8_lossy(&received.stderr));
	assert!(String::from_utf8_lossy(&received.stdout).contains("scuttle-1 spendable: 100000"),
		"external balance: {}", String::from_utf8_lossy(&received.stdout));
	let return_address = sender.new_address().await.unwrap();
	let sent = run("send", Some(return_address.to_string())).output().await.expect("external send");
	assert!(sent.status.success(), "external send: {}", String::from_utf8_lossy(&sent.stderr));
	assert!(String::from_utf8_lossy(&sent.stdout).contains("scuttle-1 sent: 50000"));
	sender.sync_mailbox().await.unwrap();
	assert!(sender.balance().await.unwrap().spendable >= sat(340_000),
		"sender did not credit the return payment");
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
