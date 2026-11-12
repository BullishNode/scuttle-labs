fn test_server_mailbox_key() -> &'static bitcoin::secp256k1::Keypair {
	static KEY: std::sync::OnceLock<bitcoin::secp256k1::Keypair> = std::sync::OnceLock::new();
	KEY.get_or_init(|| bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[251; 32]).unwrap())
}

pub(crate) async fn payout_address(
	srv: &Captaind, policy: VtxoPolicy, output_key: &bitcoin::secp256k1::Keypair,
	mailbox: ark::mailbox::MailboxIdentifier,
) -> String {
	let info = srv.ark_info().await;
	ark::Address::builder()
		.testnet(srv.config().network != bitcoin::Network::Bitcoin)
		.server_pubkey(info.server_pubkey)
		.policy(policy)
		.mailbox(info.mailbox_pubkey, mailbox, output_key).unwrap()
		.into_address().unwrap().to_string()
}

pub(crate) fn blinded_mailbox(address: &str) -> Vec<u8> {
	let address: ark::Address = address.parse().unwrap();
	match address.delivery() {
		[ark::address::VtxoDelivery::ServerMailbox { blinded_id }] => blinded_id.as_ref().to_vec(),
		_ => panic!("test address needs one server mailbox"),
	}
}

/// Give hand-built outputs the mailbox selected by their creator.
async fn signed_package(
	srv: &Captaind, wallets: &[&Wallet], mut package: protos::ArkoorPackageCosignRequest,
) -> protos::ArkoorPackageCosignRequest {
	if !srv.config().expiry_payouts { return package; }
	let mut addresses = std::collections::BTreeMap::new();
	for output in package.parts.iter().flat_map(|part| part.outputs.iter().chain(&part.isolated_outputs)) {
		if !output.blinded_mailbox_id.is_empty() { continue; }
		let policy = VtxoPolicy::deserialize(&output.policy).unwrap();
		let key = policy.user_pubkey();
		if addresses.contains_key(&policy.serialize()) { continue; }
		let mut owner = None;
		for wallet in wallets {
			if let Some((_, keypair)) = wallet.pubkey_keypair(&key).await.unwrap() {
				owner = Some((keypair, wallet.mailbox_identifier()));
				break;
			}
		}
		let (keypair, mailbox) = owner.unwrap_or_else(|| panic!("hand-built output {key} needs its keypair"));
		addresses.insert(policy.serialize(), payout_address(srv, policy, &keypair, mailbox).await);
	}
	for output in package.parts.iter_mut().flat_map(|part| part.outputs.iter_mut().chain(&mut part.isolated_outputs)) {
		if !output.blinded_mailbox_id.is_empty() { continue; }
		output.blinded_mailbox_id = blinded_mailbox(&addresses[&output.policy]);
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
