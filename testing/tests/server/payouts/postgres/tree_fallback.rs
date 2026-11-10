#[tokio::test]
async fn customer_births_record_their_owners_once() {
	let (_ctx, db) = test_db("customer_birth_owners").await;
	let server = super::super::test_server_mailbox_key();
	// SQL role fixtures use one explicit mailbox for each coin birth.
	let births = [
		(&VTXO_VECTORS.board_vtxo, SpendState::Spendable),
		(&VTXO_VECTORS.arkoor2_vtxo, SpendState::Unregistered),
		(&VTXO_VECTORS.round1_vtxo, SpendState::Unclaimed),
		(&VTXO_VECTORS.arkoor_htlc_out_vtxo, SpendState::HtlcRecvUnclaimed),
	];
	for (coin, state) in births {
		let coin = ServerVtxo::from(coin.clone());
		assert_eq!(db.write(async |tx| tx.execute_vtxo_tree_update(
			VtxoTreeUpdate::new().insert_unspent_vtxos([coin.clone()], state)
				.bind_owner_mailboxes([(coin.id(), server.public_key())])?).await).await.unwrap(), 1);
		let owner = db.read(async |tx| Ok(tx.query_one("SELECT mailbox_pk,xmin::TEXT FROM coin_owner WHERE vtxo_id=$1",
			&[&coin.id().to_string()]).await?)).await.unwrap();
		assert_eq!(owner.get::<_, &str>(0), server.public_key().to_string());
		let original_xmin = owner.get::<_, String>(1);
		assert_eq!(db.write(async |tx| tx.execute_vtxo_tree_update(
			VtxoTreeUpdate::new().insert_unspent_vtxos([coin.clone()], state)
				.bind_owner_mailboxes([(coin.id(), server.public_key())])?).await).await.unwrap(), 0);
		assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT xmin::TEXT FROM coin_owner WHERE vtxo_id=$1",
			&[&coin.id().to_string()]).await?.get::<_, String>(0))).await.unwrap(), original_xmin);
	}
	// An unbound customer birth is refused before either row is stored.
	// An explicit binding admits the birth; replay preserves it.
	let prior = ServerVtxo::from(VTXO_VECTORS.arkoor3_vtxo.clone());
	db.write(async |tx| tx.execute_vtxo_tree_update(
		VtxoTreeUpdate::new().insert_spendable_vtxos([prior.clone()])).await).await.unwrap_err();
	assert!(db.read(async |tx| tx.try_get_bare_vtxo_by_id(prior.id()).await).await.unwrap().is_none());
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT count(*) FROM coin_owner WHERE vtxo_id=$1",
		&[&prior.id().to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
	db.write(async |tx| {
		let update = || VtxoTreeUpdate::new().insert_spendable_vtxos([prior.clone()])
			.bind_owner_mailboxes([(prior.id(), server.public_key())]);
		assert_eq!(tx.execute_vtxo_tree_update(update()?).await?, 1);
		let original_xmin = tx.query_one("SELECT xmin::TEXT FROM coin_owner WHERE vtxo_id=$1",
			&[&prior.id().to_string()]).await?.get::<_, String>(0);
		assert_eq!(tx.execute_vtxo_tree_update(update()?).await?, 0);
		assert_eq!(tx.query_one("SELECT count(*) FROM coin_owner WHERE vtxo_id=$1",
			&[&prior.id().to_string()]).await?.get::<_, i64>(0), 1);
		assert_eq!(tx.query_one("SELECT xmin::TEXT FROM coin_owner WHERE vtxo_id=$1",
			&[&prior.id().to_string()]).await?.get::<_, String>(0), original_xmin);
		Ok(())
	}).await.unwrap();
}

#[tokio::test]
async fn derived_expiry_owner_uses_the_stage_selected_key() {
	let (_ctx, db) = test_db("derived_expiry_stage_owner").await;
	let server = super::super::test_server_mailbox_key();
	let coin = ServerVtxo::from(VTXO_VECTORS.board_vtxo.clone());
	db.write(async |tx| tx.execute_vtxo_tree_update(VtxoTreeUpdate::new()
		.insert_spendable_vtxos([coin.clone()])
		.bind_owner_mailboxes([(coin.id(), server.public_key())])?).await).await.unwrap();
	let mut proposal = OwnershipProposal {
		id: coin.id(), gross: coin.amount(), disposition: OwnershipDisposition::Customer { user_pubkey: coin.policy().user_pubkey().unwrap() },
		binding_source: OwnershipBindingSource::Coin(coin.id()),
		evidence: Some(OwnershipEvidence::Pubkey), cancellations: Vec::new(),
		needs_chain_progress: false, settlement_integrity_hold: false,
	};
	let original = Attribution::Owned { mailbox_pk: server.public_key(),
		spk: bitcoin::ScriptBuf::new_p2tr(&ark::SECP, server.x_only_public_key().0, None) };
	assert_eq!(db.read(async |tx| tx.derived_expiry_owner(&proposal).await).await.unwrap(), original);

	// A different P cannot borrow this coin's mailbox, and a different coin
	// id cannot borrow the first coin's binding.
	proposal.disposition = OwnershipDisposition::Customer { user_pubkey:
		bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[37; 32]).unwrap().public_key() };
	assert_eq!(db.read(async |tx| tx.derived_expiry_owner(&proposal).await).await.unwrap(), Attribution::Unresolved);
	proposal.disposition = OwnershipDisposition::Customer { user_pubkey: coin.policy().user_pubkey().unwrap() };
	proposal.binding_source = OwnershipBindingSource::Coin(VTXO_VECTORS.arkoor3_vtxo.id());
	assert_eq!(db.read(async |tx| tx.derived_expiry_owner(&proposal).await).await.unwrap(), Attribution::Unresolved);
	proposal.binding_source = OwnershipBindingSource::Coin(coin.id());
	proposal.evidence = None;
	assert_eq!(db.read(async |tx| tx.derived_expiry_owner(&proposal).await).await.unwrap(), Attribution::Unresolved);

	proposal.evidence = Some(OwnershipEvidence::Pubkey);
	// Admission is authoritative; payout resolves the stored coin binding.
	assert_eq!(db.read(async |tx| tx.derived_expiry_owner(&proposal).await).await.unwrap(), original);
}

#[tokio::test]
async fn one_coin_key_can_have_distinct_mailboxes_on_distinct_births() {
	let (_ctx, db) = test_db("same_key_distinct_coin_mailboxes").await;
	let first = ServerVtxo::from(VTXO_VECTORS.board_vtxo.clone());
	let second_point = bitcoin::OutPoint::new(first.point().txid, first.point().vout + 1000);
	let second = ServerVtxo::new(second_point, first.policy().clone(), first.amount(),
		first.expiry_height(), first.server_pubkey(), first.exit_delta(), second_point);
	// A direct chain-anchor output has an empty genesis chain.
	let second = ServerVtxo::<ark::vtxo::Full>::deserialize_with_genesis(&second.serialize(), &[0]).unwrap();
	let first_mailbox = bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[37; 32]).unwrap().public_key();
	let second_mailbox = bitcoin::secp256k1::Keypair::from_seckey_slice(&ark::SECP, &[38; 32]).unwrap().public_key();
	for (coin, mailbox) in [(first.clone(), first_mailbox), (second.clone(), second_mailbox)] {
		db.write(async |tx| tx.execute_vtxo_tree_update(VtxoTreeUpdate::new()
			.insert_spendable_vtxos([coin.clone()])
			.bind_owner_mailboxes([(coin.id(), mailbox)])?).await).await.unwrap();
	}
	let key = first.policy().user_pubkey().unwrap();
	assert_eq!(second.policy().user_pubkey(), Some(key));
	for (id, gross, mailbox) in [(first.id(), first.amount(), first_mailbox), (second.id(), second.amount(), second_mailbox)] {
		let proposal = OwnershipProposal {
			id, gross, disposition: OwnershipDisposition::Customer { user_pubkey: key },
			binding_source: OwnershipBindingSource::Coin(id), evidence: Some(OwnershipEvidence::Pubkey),
			cancellations: Vec::new(), needs_chain_progress: false, settlement_integrity_hold: false,
		};
		assert_eq!(db.read(async |tx| tx.derived_expiry_owner(&proposal).await).await.unwrap(),
			Attribution::Owned { mailbox_pk: mailbox, spk: ark::fallback::payout_script(mailbox) });
	}
}

#[tokio::test]
async fn customer_birth_constraint_is_deferred_and_padding_is_exact() {
	let (_ctx, db) = test_db("deferred_customer_birth_owner").await;
	let coin = ServerVtxo::from(VTXO_VECTORS.board_vtxo.clone());
	let mailbox = super::super::test_server_mailbox_key().public_key();
	for state in [SpendState::Spendable, SpendState::Unregistered, SpendState::Unclaimed, SpendState::HtlcRecvUnclaimed] {
		let missing = db.write(async |tx| {
			assert_eq!(tx.execute_vtxo_tree_update(VtxoTreeUpdate::new().insert_unspent_vtxos([coin.clone()], state)
				.bind_owner_mailboxes([(coin.id(), mailbox)])?).await?, 1);
			// The INSERT succeeded before its paired row was removed. Only
			// the deferred commit check may reject this transaction.
			tx.execute("DELETE FROM coin_owner WHERE vtxo_id=$1", &[&coin.id().to_string()]).await?;
			Ok(())
		}).await.unwrap_err();
		assert_eq!(missing.to_string(), "db tx commit error");
		let error = missing.downcast_ref::<tokio_postgres::Error>().unwrap().as_db_error().unwrap();
		assert_eq!(error.code(), &tokio_postgres::error::SqlState::CHECK_VIOLATION);
		assert_eq!(error.message(), "customer VTXO has no coin_owner");
		assert!(db.read(async |tx| tx.try_get_bare_vtxo_by_id(coin.id()).await).await.unwrap().is_none());
	}
	let pool = ServerVtxo::from(VTXO_VECTORS.round2_vtxo.clone());
	db.write(async |tx| tx.execute_vtxo_tree_update(VtxoTreeUpdate::new().insert_unspent_vtxos([pool], SpendState::Pool)).await)
		.await.unwrap();

	let base = &VTXO_VECTORS.board_vtxo;
	let policy = ark::VtxoPolicy::new_pubkey("02dfa52f6690299d2d6a08323083e290597b56fee125063e5f4e2957731639c42c".parse().unwrap());
	let point = bitcoin::OutPoint::new(base.point().txid, 1000);
	let padding = ServerVtxo::new(point, policy.into(), base.amount(), base.expiry_height(),
		base.server_pubkey(), base.exit_delta(), base.chain_anchor());
	// A direct SQL role fixture exercises only the constraint's exact trusted
	// padding exception, not a funded/signed native round or an issuing RPC.
	db.write(async |tx| Ok(tx.execute("INSERT INTO vtxo
		(vtxo_id,vtxo_txid,vtxo,expiry,exit_delta,policy_type,policy,server_pubkey,amount,anchor_point,spend_state,created_at,updated_at)
		VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'unclaimed',NOW(),NOW())", &[
			&padding.id().to_string(), &padding.point().txid.to_string(), &padding.serialize(),
			&(padding.expiry_height().to_u32() as i32), &(padding.exit_delta().to_u16() as i32),
			&padding.policy_type().to_string(), &padding.policy().serialize(), &padding.server_pubkey().to_string(),
			&(padding.amount().to_sat() as i64), &padding.chain_anchor().to_string(),
		]).await?)).await.unwrap();
	assert_eq!(db.read(async |tx| Ok(tx.query_one("SELECT count(*) FROM coin_owner WHERE vtxo_id=$1",
		&[&padding.id().to_string()]).await?.get::<_, i64>(0))).await.unwrap(), 0);
}

/// An undone round resets its inputs, except one the expiry decision already
/// entitled: that coin stays spent, so it can never be spent a second time.
#[tokio::test]
async fn undone_round_keeps_a_decided_input_spent() {
	let (_ctx, db) = test_db("undo_round_decided_input").await;
	let decided = ServerVtxo::from(VTXO_VECTORS.board_vtxo.clone());
	let other = ServerVtxo::from(VTXO_VECTORS.round1_vtxo.clone());
	let mailbox = super::super::test_server_mailbox_key().public_key();
	db.write(async |tx| tx.execute_vtxo_tree_update(VtxoTreeUpdate::new()
		.insert_spendable_vtxos([decided.clone(), other.clone()])
		.bind_owner_mailboxes([(decided.id(), mailbox), (other.id(), mailbox)])?).await).await.unwrap();
	let round = db.write(async |tx| Ok(tx.query_one("INSERT INTO round(seq,funding_txid,funding_tx,signed_tree,expiry,created_at) \
		VALUES(1,'undo-fixture','\\x00','\\x00',1,NOW()) RETURNING id", &[]).await?.get::<_, i64>(0))).await.unwrap();
	db.write(async |tx| tx.execute_vtxo_tree_update(VtxoTreeUpdate::new()
		.mark_vtxos_round_spent([(decided.id(), round), (other.id(), round)])).await).await.unwrap();
	db.write(async |tx| Ok(tx.execute("INSERT INTO expiry_entitlement(vtxo_id) VALUES($1)",
		&[&decided.id().to_string()]).await?)).await.unwrap();
	db.write(async |tx| tx.execute_vtxo_tree_update(VtxoTreeUpdate::new().undo_round(round)).await).await.unwrap();
	let state = async |id| db.read(async |tx| tx.get_user_vtxos_by_id(&[id]).await).await.unwrap().remove(0);
	let decided = state(decided.id()).await;
	assert_eq!((decided.spend_state, decided.spent_in_round), (SpendState::Spent, Some(round)));
	let other = state(other.id()).await;
	assert_eq!((other.spend_state, other.spent_in_round), (SpendState::Spendable, None));
}
