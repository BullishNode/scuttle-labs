use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use ark::Address;
use bark::{Config, OpenWalletArgs, Wallet, WalletSeed};
use bitcoin::{Amount, Network};

#[tokio::main]
async fn main() -> Result<()> {
	let mut config = Config::network_default(Network::Regtest);
	config.server_address = std::env::var("SCUTTLE_ARK_URL")?;
	config.bitcoind_address = Some(std::env::var("SCUTTLE_BITCOIND_URL")?);
	config.bitcoind_cookiefile = Some(PathBuf::from(std::env::var("SCUTTLE_BITCOIND_COOKIE")?));
	config.require_scuttle_payouts = true;

	let wallet = Wallet::open(
		Network::Regtest,
		WalletSeed::new_from_seed(Network::Regtest, &[42; 64]),
		config,
		OpenWalletArgs {
			datadir: Some(PathBuf::from(std::env::var("SCUTTLE_WALLET_DIR")?)),
			run_daemon: false,
			skip_recovery: true,
			..Default::default()
		},
	).await.context("open pinned Scuttle wallet")?;
	wallet.refresh_server().await.context("check paired Scuttle server")?;
	match std::env::var("SCUTTLE_ACTION").as_deref().unwrap_or("address") {
		"address" => {
			let address = wallet.new_address().await.context("create upstream-format Ark address")?;
			println!("scuttle-1 wallet open: {address}");
		},
		"sync" => {
			wallet.sync_mailbox().await.context("sync received Ark coins")?;
			wallet.sync_fallback_payouts().await.context("sync attested fallback status")?;
			println!("scuttle-1 spendable: {}", wallet.balance().await?.spendable.to_sat());
		},
		"send" => {
			let address = Address::from_str(&std::env::var("SCUTTLE_SEND_ADDRESS")?)?;
			let amount = std::env::var("SCUTTLE_SEND_SATS")?.parse()?;
			wallet.send_arkoor_payment(&address, Amount::from_sat(amount)).await
				.context("send through pinned Scuttle SDK")?;
			println!("scuttle-1 sent: {amount}");
		},
		other => anyhow::bail!("unknown SCUTTLE_ACTION: {other}"),
	}
	Ok(())
}
