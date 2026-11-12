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
