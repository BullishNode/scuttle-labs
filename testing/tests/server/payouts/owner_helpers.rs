use ark::lightning::PaymentHash;
use server::database::Db;

pub(super) async fn owner_rows(db: &Db) -> Vec<(String, String)> {
	db.read(async |tx| Ok(tx.query("SELECT vtxo_id,mailbox_pk \
		FROM coin_owner ORDER BY vtxo_id", &[]).await?
		.into_iter().map(|row| (row.get(0), row.get(1))).collect())).await.unwrap()
}

pub(super) async fn source_rows(db: &Db, ids: &[ark::VtxoId]) -> Vec<String> {
	let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>();
	db.read(async |tx| Ok(tx.query("SELECT to_jsonb(v)::TEXT FROM vtxo v \
		WHERE vtxo_id=ANY($1) ORDER BY vtxo_id", &[&ids]).await?
		.into_iter().map(|row| row.get(0)).collect())).await.unwrap()
}

pub(super) fn vtxo_replay_rows(mut rows: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
	for row in &mut rows { assert!(row.as_object_mut().unwrap().remove("updated_at").is_some()); }
	rows
}

pub(super) async fn receive_admission_rows(db: &Db, hash: PaymentHash) -> Vec<String> {
	let hash = hash.to_string();
	let mut rows = db.read(async |tx| {
		let row = tx.query_one("SELECT \
			(SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY s.id),'[]')::TEXT FROM lightning_htlc_subscription s WHERE payment_hash=$1), \
			(SELECT COALESCE(jsonb_agg(to_jsonb(h) ORDER BY h.id),'[]')::TEXT FROM htlc_vtxo h WHERE payment_hash=$1), \
			(SELECT COALESCE(jsonb_agg(to_jsonb(v) ORDER BY v.vtxo_id),'[]')::TEXT FROM vtxo v JOIN htlc_vtxo h ON h.id=v.id WHERE h.payment_hash=$1), \
			(SELECT COUNT(*)::TEXT FROM htlc_settlement WHERE payment_hash=$1), \
			(SELECT COALESCE(jsonb_agg(to_jsonb(c) ORDER BY c.source_vtxo_id),'[]')::TEXT FROM expiry_lightning_receive_claim c WHERE payment_hash=$1)", &[&hash]).await?;
		Ok((0..5).map(|index| row.get::<_, String>(index)).collect::<Vec<_>>())
	}).await.unwrap();
	rows.push(format!("{:?}", owner_rows(db).await));
	rows
}
