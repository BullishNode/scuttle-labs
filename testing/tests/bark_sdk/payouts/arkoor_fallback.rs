/// A recipient without a server mailbox cannot receive a payable coin.
/// An altered but valid address can bind the recipient's coin key to the
/// payer's mailbox. The recipient must refuse that coin even with its id.
#[tokio::test]
async fn altered_address_is_not_credited_by_attested_recovery() {
	let ctx = TestContext::new("bark_sdk/altered_address_attested_recovery_refusal").await;
	let srv = ctx.captaind("server").cfg(|cfg| cfg.expiry_payouts = true).create().await;
	let sender = ctx.bark_sdk("sender", &srv).boarded(sat(400_000))
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	let recipient = ctx.bark_sdk("recipient", &srv)
		.cfg(|cfg| cfg.daemon_manual_sync = true).create().await;
	sender.stop_daemon_wait().await.unwrap();
	recipient.stop_daemon_wait().await.unwrap();
	let address = recipient.new_address().await.unwrap();
	let [VtxoDelivery::ServerMailbox { blinded_id }] = address.delivery() else {
		panic!("recipient address must carry one mailbox")
	};
	let changed_point = PublicKey::combine_keys(&[
		&blinded_id.as_pubkey(), &sender.mailbox_identifier().as_pubkey(),
		&recipient.mailbox_identifier().as_pubkey().negate(&ark::SECP),
	]).unwrap();
	let altered = ark::Address::new(address.is_testnet(), address.ark_id(),
		address.policy().clone(), vec![VtxoDelivery::ServerMailbox {
			blinded_id: BlindedMailboxIdentifier::from_pubkey(changed_point),
		}]);
	sender.send_arkoor_payment(&altered, sat(100_000)).await.unwrap();
	let db = Db::connect(&srv.config().postgres).await.unwrap();
	let rows = db.read(async |tx| Ok(tx.query("SELECT vtxo_id, policy FROM vtxo", &[]).await?))
		.await.unwrap();
	let id = rows.iter().find_map(|row| {
		let policy = ark::ServerVtxoPolicy::deserialize(row.get::<_, &[u8]>("policy")).ok()?;
		(policy.user_pubkey() == Some(address.policy().user_pubkey()))
			.then(|| row.get::<_, &str>("vtxo_id").parse::<ark::VtxoId>().unwrap())
	}).expect("server stored recipient-keyed coin");
	let (_, key) = recipient.pubkey_keypair(&address.policy().user_pubkey()).await.unwrap().unwrap();
	let mut rpc = srv.get_public_rpc().await;
	let status = rpc.get_vtxo_status(protos::GetVtxoStatusRequest {
		vtxo_id: id.to_bytes().to_vec(),
		attestation: VtxoStatusAttestation::new(id, &key).serialize(),
	}).await.unwrap().into_inner();
	assert_eq!(status.owner_mailbox, sender.mailbox_identifier().serialize());
	assert_ne!(status.owner_mailbox, recipient.mailbox_identifier().serialize());
	recipient.sync_mailbox().await.unwrap();
	assert!(recipient.spendable_vtxos().await.unwrap().is_empty());
	let report = recipient.recover_vtxos([id], None).await.unwrap();
	assert!(report.recovered().is_empty(), "{report:?}");
	assert!(recipient.spendable_vtxos().await.unwrap().is_empty());
	assert_eq!(report.foreign().ids().collect::<Vec<_>>(), [id]);
	let body = db.read(async |tx| tx.get_user_vtxos_by_id(&[id]).await).await.unwrap().remove(0).vtxo;
	let error = recipient.import_vtxo(&body, Default::default()).await.unwrap_err();
	assert!(format!("{error:#}").contains("another mailbox"), "{error:#}");
	assert!(recipient.spendable_vtxos().await.unwrap().is_empty());
}
