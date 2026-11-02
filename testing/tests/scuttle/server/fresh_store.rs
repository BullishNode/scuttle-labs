use ark_testing::TestContext;
use server::database::Db;

/// A retired diagnostic table is refused, not dropped during a reopen.
#[tokio::test]
async fn hold_bearing_scuttle_store_is_refused_without_history_changes() {
	let mut ctx = TestContext::new_minimal("postgresd/hold_bearing_scuttle_store_refused").await;
	ctx.init_central_postgres().await;
	let cfg = ctx.new_postgres(&ctx.test_name).await;
	drop(Db::create(&cfg).await.unwrap());
	let client = ctx.postgres_manager().database_client(Some(&cfg.name)).await;
	client.batch_execute("CREATE TABLE scuttle.expiry_hold (
		vtxo_id TEXT PRIMARY KEY REFERENCES public.vtxo(vtxo_id), reason TEXT NOT NULL,
		first_seen TIMESTAMPTZ NOT NULL DEFAULT NOW(), last_seen TIMESTAMPTZ NOT NULL DEFAULT NOW()
	);").await.unwrap();
	let history = "SELECT version,name,checksum FROM public.refinery_schema_history ORDER BY version";
	let before = client.query(history, &[]).await.unwrap().into_iter()
		.map(|row| (row.get::<_, i32>(0),row.get::<_, String>(1),row.get::<_, String>(2))).collect::<Vec<_>>();
	let error = match Db::connect(&cfg).await {
		Ok(_) => panic!("hold-bearing Scuttle shape must be refused"), Err(error) => error,
	};
	assert!(format!("{error:#}").contains("incomplete Scuttle schema"));
	let after = client.query(history, &[]).await.unwrap().into_iter()
		.map(|row| (row.get::<_, i32>(0),row.get::<_, String>(1),row.get::<_, String>(2))).collect::<Vec<_>>();
	assert_eq!(before,after);
	assert!(client.query_one("SELECT to_regclass('scuttle.expiry_hold') IS NOT NULL", &[]).await.unwrap().get::<_, bool>(0));
}
