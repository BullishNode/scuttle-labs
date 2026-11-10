fn test_server_mailbox_key() -> &'static bitcoin::secp256k1::Keypair {
	static KEY: std::sync::OnceLock<bitcoin::secp256k1::Keypair> = std::sync::OnceLock::new();
	KEY.get_or_init(|| bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[251; 32]).unwrap())
}

/// Give hand-built outputs the mailbox selected by their creator.
async fn signed_package(
	srv: &Captaind, wallets: &[&Wallet], mut package: protos::ArkoorPackageCosignRequest,
) -> protos::ArkoorPackageCosignRequest {
	if !srv.config().expiry_payouts { return package; }
	let mut mailboxes = std::collections::BTreeMap::new();
	for output in package.parts.iter().flat_map(|part| part.outputs.iter().chain(&part.isolated_outputs)) {
		if output.address.is_some() || output.mailbox_id.is_some() { continue; }
		let key = VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey();
		if mailboxes.contains_key(&key) { continue; }
		let mut mailbox = None;
		for wallet in wallets {
			if wallet.pubkey_keypair(&key).await.unwrap().is_some() {
				mailbox = Some(wallet.mailbox_identifier());
				break;
			}
		}
		mailboxes.insert(key, mailbox.unwrap_or_else(|| panic!("hand-built output {key} needs its mailbox")));
	}
	for output in package.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		if output.address.is_some() || output.mailbox_id.is_some() { continue; }
		let key = VtxoPolicy::deserialize(&output.policy).unwrap().user_pubkey();
		output.mailbox_id = Some(mailboxes[&key].serialize());
	}
	package
}

/// The key a wallet bound its receive to, with its secret, for tests that
/// prepare the claim by hand.
async fn receive_claim_keypair(srv: &Captaind, wallet: &Wallet, hash: ark::lightning::PaymentHash) -> Keypair {
	let db = server::database::Db::connect(&srv.config().postgres).await.unwrap();
	let sub = db.read(async |t| t.get_htlc_subscription_by_payment_hash(hash).await).await.unwrap().unwrap();
	let key = sub.claim_pubkey.expect("the wallet bound its receive to a claim key");
	wallet.pubkey_keypair(&key).await.unwrap().expect("the claim key is the wallet's").1
}
