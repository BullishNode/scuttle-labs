/// The allocation winner is chosen before matching the queried key or mailbox.
/// These populated rows exercise all three authenticated status APIs.
#[tokio::test]
async fn recovery_pending_customer_uses_immutable_mailbox_after_allocation_ordering() {
	let ctx = TestContext::new("server/recovery_pending_customer_ordering").await;
	let srv = ctx.captaind("server").no_vtxo_pool().create().await;
	let db = Db::connect(&srv.config().postgres, bitcoin::Network::Regtest).await.unwrap();
	let xpriv = dummy_xpriv(NetworkKind::Test);
	let keys = (0_u32..6).map(|index| xpriv.derive_priv(&SECP, &[index.into()]).unwrap().to_keypair(&SECP))
		.collect::<Vec<_>>();
	let mailbox = super::test_server_mailbox_key();
	let foreign = Keypair::new(&SECP, &mut bitcoin::secp256k1::rand::thread_rng());
	super::register_test_owners(&db, &srv.mailbox_keypair(), mailbox,
		&[keys[0], keys[1], keys[3], keys[4]]).await;
	super::register_test_owners(&db, &srv.mailbox_keypair(), &foreign, &[keys[2]]).await;
	let coins = keys.iter().enumerate().map(|(index, key)| DummyTestVtxoSpec {
		amount: Amount::from_sat(100_000 + index as u64), fee: Amount::ZERO, user_keypair: *key, ..Default::default()
	}.build().1).collect::<Vec<_>>();
	db.write(async |tx| {
		tx.upsert_vtxos(coins[..5].iter().cloned().map(ServerVtxo::from)).await?;
		tx.execute_vtxo_tree_update(server::database::tree::VtxoTreeUpdate::new()
			.insert_unspent_vtxos([ServerVtxo::from(coins[5].clone())], server::database::SpendState::Pool)).await?;
		let ids = coins[..5].iter().map(|coin| coin.id().to_string()).collect::<Vec<_>>();
		tx.execute("UPDATE vtxo SET spend_state='spent' WHERE vtxo_id=ANY($1)", &[&ids]).await?;
		Ok(())
	}).await.unwrap();
	let allocation = db.write(async |tx| {
		let allocation: i64 = tx.query_one("INSERT INTO expiry_allocation(expiry_height,status) \
			VALUES(1,'pending') RETURNING id", &[]).await?.get("id");
		let script = bitcoin::ScriptBuf::new_p2tr(&ark::SECP, mailbox.x_only_public_key().0, None);
		for (index, coin) in coins[..4].iter().enumerate() {
			let entitlement: i64 = tx.query_one("INSERT INTO expiry_entitlement(vtxo_id,gross_sat) VALUES($1,$2) RETURNING id",
				&[&coin.id().to_string(), &i64::try_from(coin.amount().to_sat())?]).await?.get("id");
			tx.execute("INSERT INTO expiry_allocation_coin(allocation_id,entitlement_id,owner_mailbox,user_pubkey,spk, \
				disposition,gross_sat,fee_sat,payout_sat,forfeited_sat,evidence) \
				VALUES($1,$2,$3,$4,$5,$6,$7,0,$8,0,'{}')", &[
				&allocation, &entitlement, &mailbox.public_key().to_string(),
				&keys[0].public_key().to_string(), &script.to_hex_string(),
				&if index == 3 { "operator" } else { "customer" },
				&i64::try_from(coin.amount().to_sat())?,
				&if index == 3 { 0_i64 } else { i64::try_from(coin.amount().to_sat())? },
			]).await?;
		}
		tx.execute("INSERT INTO expiry_allocation_payout(allocation_id,output_index,spk,amount_sat)
			VALUES($1,0,$2,$3)", &[&allocation,&script.to_hex_string(),
			&i64::try_from(coins[..3].iter().map(|coin| coin.amount()).sum::<Amount>().to_sat())?]).await?;

		Ok(allocation)
	}).await.unwrap();
	let expected = [Some(protos::VtxoSpendState::FallbackPending), Some(protos::VtxoSpendState::FallbackPending),
		Some(protos::VtxoSpendState::Spent), Some(protos::VtxoSpendState::Spent), Some(protos::VtxoSpendState::Spent), None];
	let owner_mailboxes = [Some(mailbox.public_key()), Some(mailbox.public_key()), Some(foreign.public_key()),
		Some(mailbox.public_key()), Some(mailbox.public_key()), None];
	assert_pending_statuses(&srv, &xpriv, &coins, &keys, &expected, &owner_mailboxes, "active-customer").await;

	// An older matching customer must not beat a later active operator.
	let replacement = db.write(async |tx| {
		tx.execute("UPDATE expiry_allocation_coin SET active=FALSE WHERE allocation_id=$1", &[&allocation]).await?;
		let replacement: i64 = tx.query_one("INSERT INTO expiry_allocation(expiry_height,status) \
			VALUES(1,'pending') RETURNING id", &[]).await?.get("id");
		tx.execute("INSERT INTO expiry_allocation_coin(allocation_id,entitlement_id,disposition,gross_sat,fee_sat,payout_sat,forfeited_sat,evidence) \
			SELECT $1,entitlement_id,'operator',gross_sat,0,0,0,'{}' FROM expiry_allocation_coin WHERE allocation_id=$2",
			&[&replacement, &allocation]).await?;
		Ok(replacement)
	}).await.unwrap();
	let spent = [Some(protos::VtxoSpendState::Spent); 5].into_iter().chain([None]).collect::<Vec<_>>();
	assert_pending_statuses(&srv, &xpriv, &coins, &keys, &spent, &owner_mailboxes, "replacement-operator").await;
	// Confirmed customer ownership wins even over that newer active row.
	db.write(async |tx| { tx.execute("UPDATE expiry_allocation SET status='confirmed' WHERE id=$1", &[&allocation]).await?; Ok(()) }).await.unwrap();
	assert_pending_statuses(&srv, &xpriv, &coins, &keys, &expected, &owner_mailboxes, "confirmed-customer").await;
	// Without a confirmed row, active outranks newer inactive ownership.
	db.write(async |tx| {
		tx.execute("UPDATE expiry_allocation SET status='conflicted' WHERE id=$1", &[&allocation]).await?;
		tx.execute("UPDATE expiry_allocation_coin SET active=FALSE WHERE allocation_id=$1", &[&replacement]).await?;
		tx.execute("UPDATE expiry_allocation_coin SET active=TRUE WHERE allocation_id=$1", &[&allocation]).await?;
		Ok(())
	}).await.unwrap();
	assert_pending_statuses(&srv, &xpriv, &coins, &keys, &expected, &owner_mailboxes, "active-beats-inactive").await;
	// Equal confirmation/active ranks choose the newest allocation, including
	// an operator; filtering to customer matches before ordering is unsafe.
	db.write(async |tx| { tx.execute("UPDATE expiry_allocation_coin SET active=FALSE WHERE allocation_id=$1", &[&allocation]).await?; Ok(()) }).await.unwrap();
	assert_pending_statuses(&srv, &xpriv, &coins, &keys, &spent, &owner_mailboxes, "newest-inactive-operator").await;
}

// Test-only legacy response shapes retain retired tag 1000. The current
// generated responses discard it, so their equality cannot detect an old
// server disclosing owners. Optional bytes also detect an explicitly empty tag.
#[derive(Clone, PartialEq, prost::Message, serde::Serialize)]
struct LegacyPublicVtxo {
	#[prost(bytes = "vec", tag = "1")]
	vtxo: Vec<u8>,
	#[prost(bytes = "vec", optional, tag = "1000")]
	owner_mailbox: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message, serde::Serialize)]
struct LegacyPublicVtxos {
	#[prost(bytes = "vec", repeated, tag = "1")]
	vtxos: Vec<Vec<u8>>,
	#[prost(bytes = "vec", repeated, tag = "2")]
	missing_vtxo_ids: Vec<Vec<u8>>,
	#[prost(message, repeated, tag = "1000")]
	owners: Vec<LegacyPublicVtxoOwner>,
}

#[derive(Clone, PartialEq, prost::Message, serde::Serialize)]
struct LegacyPublicVtxoOwner {
	#[prost(bytes = "vec", tag = "1000")]
	vtxo_id: Vec<u8>,
	#[prost(bytes = "vec", tag = "1001")]
	owner_mailbox: Vec<u8>,
}

async fn assert_pending_statuses(
	srv: &ark_testing::Captaind, xpriv: &Xpriv, coins: &[Vtxo<Full>], keys: &[Keypair],
	expected: &[Option<protos::VtxoSpendState>], owner_mailboxes: &[Option<PublicKey>],
	observation: &str,
) {
	let mut rpc = srv.get_public_rpc().await;
	let channel = tonic::transport::Channel::from_shared(srv.ark_url()).unwrap().connect().await.unwrap();
	let mut public = tonic::client::Grpc::new(channel);
	let mut single = Vec::new();
	for coin in coins {
		public.ready().await.unwrap();
		let mut request = tonic::Request::new(protos::GetVtxoRequest { vtxo_id: coin.id().to_bytes().to_vec() });
		request.set_pver(server_rpc::pver::PROTOCOL_VERSION_SCUTTLE);
		let response: LegacyPublicVtxo = public.unary(request,
			tonic::codegen::http::uri::PathAndQuery::from_static("/bark_server.ArkService/GetVtxo"),
			tonic_prost::ProstCodec::default()).await.unwrap().into_inner();
		single.push(response);
	}
	let unknown = VtxoId::from_slice(&[0u8; 36]).unwrap();
	assert!(coins.iter().all(|coin| coin.id() != unknown));
	let mut ids = coins.iter().map(|coin| coin.id().to_bytes().to_vec()).collect::<Vec<_>>();
	ids.push(ids[0].clone());
	ids.insert(0, unknown.to_bytes().to_vec());
	ids.push(unknown.to_bytes().to_vec());
	public.ready().await.unwrap();
	let mut request = tonic::Request::new(protos::GetVtxosByIdsRequest { vtxo_ids: ids.clone() });
	request.set_pver(server_rpc::pver::PROTOCOL_VERSION_SCUTTLE);
	let full: LegacyPublicVtxos = public.unary(request,
		tonic::codegen::http::uri::PathAndQuery::from_static("/bark_server.ArkService/GetVtxosByIds"),
		tonic_prost::ProstCodec::default()).await.unwrap().into_inner();
	// Retain BOTH actual decoded endpoint replies before either absence oracle,
	// including the old-server RED. These are not synthetic or raw-wire bytes.
	let evidence = std::fs::OpenOptions::new().write(true).create_new(true)
		.open(srv.config().data_dir.join(format!("public-body-legacy-{observation}.json"))).unwrap();
	serde_json::to_writer_pretty(evidence, &serde_json::json!({
		"single_vtxo_ids": coins.iter().map(|coin| coin.id().to_bytes().to_vec()).collect::<Vec<_>>(),
		"single_replies": &single, "batch_vtxo_ids": &ids, "batch_reply": &full,
	})).unwrap();
	for (coin, response) in coins.iter().zip(&single) {
		assert_eq!(response.vtxo, coin.serialize(), "public lookup preserves the complete signed body");
		assert!(response.owner_mailbox.is_none(), "public signed body discloses legacy mailbox tag 1000");
	}
	assert_eq!(full.vtxos, coins.iter().chain([&coins[0]]).map(ProtocolEncoding::serialize).collect::<Vec<_>>(),
		"public batch preserves exact signed bodies, order and duplicates");
	assert_eq!(full.missing_vtxo_ids, vec![unknown.to_bytes().to_vec()], "unknown ids are reported once");
	assert!(full.owners.is_empty(), "public batch discloses legacy owner tag 1000");
	let mut queries = Vec::new();
	for (((coin, key), expected), owner) in coins.iter().zip(keys).zip(expected).zip(owner_mailboxes) {
		let owner = owner.map(|key| key.serialize().to_vec()).unwrap_or_default();
		let query = protos::VtxoStatusQuery { vtxo_id: coin.id().to_bytes().to_vec(),
			attestation: VtxoStatusAttestation::new(coin.id(), key).serialize() };
		let response = rpc.get_vtxo_status(protos::GetVtxoStatusRequest {
			vtxo_id: query.vtxo_id.clone(), attestation: query.attestation.clone(),
		}).await;
		match expected {
			Some(state) => {
				let response = response.unwrap().into_inner();
				assert_eq!(response.spend_state, *state as i32);
				assert_eq!(response.owner_mailbox, owner);
			},
			None => assert_eq!(response.unwrap_err().code(), tonic::Code::NotFound),
		}
		queries.push(query);
	}
	let batch = rpc.get_vtxos_status_by_ids(protos::GetVtxosStatusByIdsRequest { vtxos: queries }).await.unwrap().into_inner();
	let recovered = rpc.recover_vtxos_by_xpub(xpub_request(xpriv, srv.ark_info().await.server_pubkey, 0))
		.await.unwrap().into_inner();
	for ((coin, expected), owner) in coins.iter().zip(expected).zip(owner_mailboxes) {
		let owner = owner.map(|key| key.serialize().to_vec()).unwrap_or_default();
		assert_eq!(batch.vtxos.iter().find(|entry| entry.vtxo_id == coin.id().to_bytes().to_vec()).map(|entry| entry.spend_state),
			expected.map(|state| state as i32));
		assert_eq!(recovered.vtxos.iter().find(|entry| Vtxo::<Full>::deserialize(&entry.vtxo).unwrap().id() == coin.id())
			.map(|entry| entry.spend_state), expected.map(|state| state as i32));
		assert_eq!(batch.missing_vtxo_ids.contains(&coin.id().to_bytes().to_vec()), expected.is_none());
		if expected.is_some() {
			assert_eq!(batch.vtxos.iter().find(|entry| entry.vtxo_id == coin.id().to_bytes().to_vec()).unwrap().owner_mailbox, owner);
			assert_eq!(recovered.vtxos.iter().find(|entry| Vtxo::<Full>::deserialize(&entry.vtxo).unwrap().id() == coin.id()).unwrap().owner_mailbox, owner);
		}
	}
}
