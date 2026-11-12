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
