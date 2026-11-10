//! Independent Bitcoin wallet used only to prove Scuttle expiry payout recovery.

use std::path::Path;

use anyhow::Context;
use bdk_wallet::chain::ChainPosition;
use bdk_wallet::signer::SignersContainer;
use bdk_wallet::{KeychainKind, Wallet};
use bitcoin::{Address, Amount, FeeRate, Network, OutPoint, PrivateKey, ScriptBuf, Txid};
use bitcoin::bip32::{DerivationPath, Xpriv};
use bitcoin::secp256k1::Keypair;
use bitcoin_ext::bdk::WalletExt;
use bitcoin_ext::rpc::RpcApi;

use crate::Bitcoind;

pub struct PayoutWallet {
	wallet: Wallet,
	signers: SignersContainer,
	script: ScriptBuf,
}

pub struct PayoutSpend {
	pub txid: Txid,
	pub output: OutPoint,
	pub fee: Amount,
}

impl PayoutWallet {
	fn mailbox_keypair(seed: &[u8; 64], network: Network) -> anyhow::Result<Keypair> {
		let master = Xpriv::new_master(network, seed)?;
		let mailbox_path = "m/350'/1'".parse::<DerivationPath>()?;
		Ok(master.derive_priv(&ark::SECP, &mailbox_path)?.to_keypair(&ark::SECP))
	}

	pub fn script_from_seed(seed: &[u8; 64], network: Network) -> anyhow::Result<ScriptBuf> {
		let keypair = Self::mailbox_keypair(seed, network)?;
		Ok(ScriptBuf::new_p2tr(&ark::SECP, keypair.x_only_public_key().0, None))
	}

	/// Create a fresh wallet after payout confirmation, from the test seed alone.
	pub fn restore(seed: &[u8; 64], network: Network, dir: &Path) -> anyhow::Result<Self> {
		std::fs::create_dir(dir).context("test payout wallet directory must be fresh")?;
		let keypair = Self::mailbox_keypair(seed, network)?;
		let script = Self::script_from_seed(seed, network)?;
		let descriptor = format!("tr({})", PrivateKey::new(keypair.secret_key(), network));
		let mut wallet = Wallet::create_single(descriptor.clone())
			.network(network).create_wallet_no_persist()?;
		let signers = wallet.build_signers(&descriptor)?;
		wallet.reveal_addresses_to(KeychainKind::External, 0).for_each(drop);
		let address = wallet.peek_address(KeychainKind::External, 0).address;
		ensure!(address.script_pubkey() == script, "test payout descriptor disagrees with tr(M)");
		std::fs::write(dir.join("watch.txt"), format!("tr({})\n{}\n",
			keypair.x_only_public_key().0, address))?;
		Ok(Self { wallet, signers, script })
	}

	pub fn script(&self) -> &ScriptBuf { &self.script }

	/// Scan from genesis, including blocks that predate this wallet's creation.
	pub fn rescan(&mut self, bitcoind: &Bitcoind) -> anyhow::Result<()> {
		let rpc = bitcoind.sync_client();
		let checkpoint = self.wallet.latest_checkpoint();
		let mut emitter = bdk_bitcoind_rpc::Emitter::new(
			&rpc, checkpoint, 0, self.wallet.unconfirmed_txs());
		while let Some(block) = emitter.next_block()? {
			self.wallet.apply_block_connected_to(
				&block.block, block.block_height(), block.connected_to())?;
		}
		self.wallet.apply_unconfirmed_txs(emitter.mempool()?.update);
		Ok(())
	}

	pub fn confirmed_payout(&self, outpoint: OutPoint, value: Amount) -> anyhow::Result<()> {
		let coin = self.wallet.list_unspent().find(|coin| coin.outpoint == outpoint)
			.context("fresh payout wallet did not discover the exact outpoint")?;
		ensure!(coin.txout.value == value, "discovered payout value differs");
		ensure!(coin.txout.script_pubkey == self.script, "discovered payout script differs");
		ensure!(matches!(coin.chain_position, ChainPosition::Confirmed { .. }),
			"discovered payout is not confirmed");
		Ok(())
	}

	/// Spend the recovered payout and write a public evidence record without keys.
	pub fn spend(&mut self, bitcoind: &Bitcoind, payout: OutPoint, value: Amount,
		destination: &Address, fee_rate: FeeRate, dir: &Path) -> anyhow::Result<PayoutSpend> {
		self.confirmed_payout(payout, value)?;
		let mut builder = self.wallet.build_tx();
		builder.drain_to(destination.script_pubkey()).drain_wallet().fee_rate(fee_rate);
		let mut psbt = builder.finish().context("building payout sweep")?;
		let finalized = self.wallet.sign_with_signers(&mut psbt, &[&self.signers],
			bdk_wallet::SignOptions::default())?;
		ensure!(finalized, "failed to sign payout sweep");
		let tx = psbt.extract_tx()?;
		ensure!(tx.input.len() == 1 && tx.input[0].previous_output == payout,
			"payout sweep did not spend only the selected outpoint");
		let fee = value.checked_sub(tx.output.iter().map(|output| output.value).sum())
			.context("payout sweep outputs exceed its input")?;
		ensure!(fee > Amount::ZERO, "payout sweep must pay a mining fee");
		let output_index = tx.output.iter().position(|output|
			output.script_pubkey == destination.script_pubkey())
			.context("payout sweep does not pay the destination")?;
		let txid = bitcoind.sync_client().send_raw_transaction(&tx)?;
		ensure!(txid == tx.compute_txid(), "broadcast changed payout sweep txid");
		let output = OutPoint::new(txid, output_index as u32);
		let evidence = serde_json::json!({
			"payout_outpoint": payout.to_string(),
			"payout_value_sat": value.to_sat(),
			"payout_script_hex": self.script.to_hex_string(),
			"spend_txid": txid.to_string(),
			"spend_output": output.to_string(),
			"spend_fee_sat": fee.to_sat(),
		});
		std::fs::write(dir.join("spend.json"), serde_json::to_vec_pretty(&evidence)?)?;
		Ok(PayoutSpend { txid, output, fee })
	}
}
