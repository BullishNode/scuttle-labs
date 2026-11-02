//! Initial Scuttle state for fresh wallets; no fork upgrade history.

use anyhow::{ensure, Context};
use rusqlite::Connection;

/// Return whether this is an empty database, refusing old stores before writes.
pub(super) fn is_fresh(conn: &Connection) -> anyhow::Result<bool> {
	let legacy: bool = conn.query_row(
		"SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' \
		 AND name IN ('scuttle_schema_history', 'bark_fallback_record', 'bark_fallback_snapshot', 'bark_fallback_receipt','bark_mailbox_identity'))", [], |row| row.get(0),
	)?;
	ensure!(!legacy, "existing wallet is not a fresh Scuttle store; initialization refused");
	let initialized: bool = conn.query_row(
		"SELECT EXISTS(SELECT 1 FROM pragma_table_info('bark_properties') WHERE name='mailbox_pubkey')", [], |row| row.get(0),
	)?;
	if !initialized {
		let occupied: bool = conn.query_row(
			"SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' \
			 AND name NOT LIKE 'sqlite_%')", [], |row| row.get(0),
		)?;
		ensure!(!occupied, "existing wallet is not a fresh Scuttle store; initialization refused");
		return Ok(true);
	}
	let current: bool = conn.query_row("SELECT
		EXISTS(SELECT 1 FROM pragma_table_info('bark_vtxo')
			WHERE name='fallback_status' AND type='TEXT' AND [notnull]=0)
		AND NOT EXISTS(SELECT 1 FROM pragma_table_info('bark_settled_lightning_receive') WHERE name='association')
		AND EXISTS(SELECT 1 FROM pragma_table_info('bark_properties') WHERE name='mailbox_pubkey' AND type='TEXT')", [], |row| row.get(0))?;
	ensure!(current, "incomplete Scuttle wallet state; use a fresh store");
	// Known current records must already exist. Reopen never repairs a partial
	// wallet by replaying upstream initialization against it.
	let complete: bool = conn.query_row("SELECT COUNT(*)=7 FROM sqlite_master WHERE type='table'
		AND name IN ('migrations','bark_properties','bark_vtxo','bark_vtxo_state',
		'bark_wallet_action_checkpoint','bark_movements',
		'bark_settled_lightning_receive')", [], |row| row.get(0))?;
	ensure!(complete, "incomplete Scuttle wallet state; use a fresh store");
	let history: bool = conn.query_row("SELECT COUNT(*)=43 AND MIN(value)=0 AND MAX(value)=42
		AND COUNT(DISTINCT value)=43 FROM migrations", [], |row| row.get(0))?;
	ensure!(history, "unrecognized wallet migration history; use a fresh Scuttle store");
	Ok(false)
}

pub(super) fn initialize(conn: &mut Connection) -> anyhow::Result<()> {
	let tx = conn.transaction()?;
	tx.execute_batch("ALTER TABLE bark_vtxo ADD COLUMN fallback_status TEXT
		CHECK (fallback_status IN ('pending','spent','forfeited'));
	ALTER TABLE bark_properties ADD COLUMN mailbox_pubkey TEXT CHECK (length(mailbox_pubkey) = 66);")
		.context("initialize fresh Scuttle wallet state")?;
	tx.commit()?;
	Ok(())
}
