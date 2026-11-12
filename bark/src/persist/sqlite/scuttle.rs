//! Scuttle's wallet tables.
//!
//! They stay outside upstream's numbered migrations, so a Scuttle wallet
//! records exactly upstream's migration versions and runs every migration
//! upstream adds later. Upstream's tables carry no Scuttle column.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::MailboxIdentity;

/// The last fallback status the server reported for each coin, and the
/// wallet's own mailbox public key. Idempotent.
const TABLES: &str = "
	CREATE TABLE IF NOT EXISTS scuttle_vtxo_fallback (
		vtxo_id TEXT PRIMARY KEY,
		status TEXT NOT NULL CHECK (status IN ('pending','spent','forfeited'))
	);
	CREATE TABLE IF NOT EXISTS scuttle_wallet_mailbox (
		id INTEGER PRIMARY KEY CHECK (id = 1),
		mailbox_pubkey TEXT NOT NULL CHECK (length(mailbox_pubkey) = 66)
	);";

/// Create the Scuttle tables. Runs on every open, after upstream's migrations.
pub(crate) fn create_tables(conn: &Connection) -> anyhow::Result<()> {
	conn.execute_batch(TABLES).context("failed to create the Scuttle wallet tables")
}

/// Move a wallet off the fork's former migration 43.
///
/// That migration added `bark_vtxo.fallback_status` and
/// `bark_properties.mailbox_pubkey` and recorded version 43, so the wallet
/// would skip an upstream migration 43. Copy both columns into the Scuttle
/// tables and forget version 43. This runs before upstream's migrations, so
/// an upstream migration 43 applies in the same open, and only once: after
/// it the Scuttle tables exist.
pub(crate) fn repair_fork_migration_43(conn: &mut Connection) -> anyhow::Result<()> {
	let tx = conn.transaction()?;
	if table_exists(&tx, "scuttle_vtxo_fallback")?
		|| !column_exists(&tx, "bark_vtxo", "fallback_status")?
	{
		return Ok(());
	}

	// SQLite before 3.35 cannot drop a column. There the old columns stay,
	// unused: upstream never names them and they accept NULL.
	let drop_columns = rusqlite::version_number() >= 3_035_000;
	tx.execute_batch(TABLES)?;
	tx.execute_batch("INSERT INTO scuttle_vtxo_fallback (vtxo_id, status)
			SELECT id, fallback_status FROM bark_vtxo WHERE fallback_status IS NOT NULL;
		DELETE FROM migrations WHERE value = 43;")?;
	if drop_columns {
		tx.execute_batch("ALTER TABLE bark_vtxo DROP COLUMN fallback_status;")?;
	}
	if column_exists(&tx, "bark_properties", "mailbox_pubkey")? {
		tx.execute_batch("INSERT INTO scuttle_wallet_mailbox (id, mailbox_pubkey)
			SELECT 1, mailbox_pubkey FROM bark_properties WHERE mailbox_pubkey IS NOT NULL;")?;
		if drop_columns {
			tx.execute_batch("ALTER TABLE bark_properties DROP COLUMN mailbox_pubkey;")?;
		}
	}
	tx.commit().context("failed to repair the fork's migration 43")
}

/// Store the mailbox key of a wallet being created.
pub(crate) fn store_mailbox_pubkey(conn: &Connection, identity: &MailboxIdentity) -> anyhow::Result<()> {
	conn.execute("INSERT INTO scuttle_wallet_mailbox (id, mailbox_pubkey) VALUES (1, ?1)",
		[identity.public_key.to_string()])?;
	Ok(())
}

/// Bind the mailbox key of a wallet that has none, or check the bound one.
pub(crate) fn bind_mailbox_identity(conn: &Connection, identity: &MailboxIdentity) -> anyhow::Result<()> {
	let (key, network) = (identity.public_key.to_string(), identity.network.to_string());
	conn.execute("INSERT OR IGNORE INTO scuttle_wallet_mailbox (id, mailbox_pubkey)
		SELECT 1, ?1 FROM bark_properties WHERE id = 1 AND network = ?2", [&key, &network])?;
	let bound: bool = conn.query_row("SELECT EXISTS (SELECT 1
		FROM scuttle_wallet_mailbox m, bark_properties p
		WHERE m.mailbox_pubkey = ?1 AND p.id = 1 AND p.network = ?2)", [&key, &network], |row| row.get(0))?;
	ensure!(bound, "mailbox identity differs from the stored wallet");
	Ok(())
}

pub(crate) fn fetch_mailbox_pubkey(conn: &Connection) -> anyhow::Result<Option<String>> {
	Ok(conn.query_row("SELECT mailbox_pubkey FROM scuttle_wallet_mailbox WHERE id = 1",
		[], |row| row.get(0)).optional()?)
}

fn table_exists(tx: &Transaction, table: &str) -> anyhow::Result<bool> {
	Ok(tx.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
		[table], |row| row.get(0))?)
}

fn column_exists(tx: &Transaction, table: &str, column: &str) -> anyhow::Result<bool> {
	Ok(tx.query_row("SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
		[table, column], |row| row.get(0))?)
}

#[cfg(test)]
mod test {
	use ark::test_util::VTXO_VECTORS;

	use crate::persist::BarkPersister;
	use crate::persist::sqlite::{query, SqliteClient};
	use crate::persist::sqlite::helpers::in_memory_db;
	use crate::persist::sqlite::migrations::MigrationContext;
	use crate::vtxo::{FallbackStatus, VtxoState};

	use super::*;

	const MAILBOX_PUBKEY: &str = "02f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";

	/// A wallet that ran the fork's former migration 43 keeps its fallback
	/// status and mailbox key, and no longer records version 43.
	#[tokio::test]
	async fn wallet_from_fork_migration_43_opens() {
		let vtxo = &VTXO_VECTORS.board_vtxo;
		let (path, mut conn) = in_memory_db();
		// Upstream's tables at version 42, then the fork's former migration 43
		// as it ran, before the Scuttle tables existed.
		MigrationContext::new().do_all_migrations(&mut conn).unwrap();
		conn.execute_batch("DROP TABLE scuttle_vtxo_fallback; DROP TABLE scuttle_wallet_mailbox;
			ALTER TABLE bark_vtxo ADD COLUMN fallback_status TEXT
				CHECK (fallback_status IN ('pending','spent','forfeited'));
			ALTER TABLE bark_properties ADD COLUMN mailbox_pubkey TEXT CHECK (length(mailbox_pubkey)=66);
			INSERT INTO migrations (value) VALUES (43);").unwrap();
		conn.execute("INSERT INTO bark_properties (id, network, fingerprint, mailbox_pubkey)
			VALUES (1, 'regtest', '00000000', ?1)", [MAILBOX_PUBKEY]).unwrap();
		let tx = conn.transaction().unwrap();
		query::store_vtxo_with_initial_state(&tx, vtxo, &VtxoState::Spent).unwrap();
		tx.execute("UPDATE bark_vtxo SET fallback_status = 'pending' WHERE id = ?1",
			[vtxo.id().to_string()]).unwrap();
		tx.commit().unwrap();

		SqliteClient::open(&path).unwrap();
		let db = SqliteClient::open(&path).unwrap();

		let coin = db.get_wallet_vtxo(vtxo.id()).await.unwrap().unwrap();
		assert_eq!(coin.state, VtxoState::Spent);
		assert_eq!(coin.fallback_status, Some(FallbackStatus::Pending));
		let identity = db.read_properties().await.unwrap().unwrap().mailbox_identity.unwrap();
		assert_eq!(identity.public_key.to_string(), MAILBOX_PUBKEY);

		let versions = conn.query_row("SELECT max(value), sum(value = 43) FROM migrations",
			[], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))).unwrap();
		assert_eq!(versions, (42, 0));
		if rusqlite::version_number() >= 3_035_000 {
			let tx = conn.transaction().unwrap();
			assert!(!column_exists(&tx, "bark_vtxo", "fallback_status").unwrap());
			assert!(!column_exists(&tx, "bark_properties", "mailbox_pubkey").unwrap());
		}
	}
}
