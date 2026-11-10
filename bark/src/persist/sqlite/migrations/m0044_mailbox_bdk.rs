use anyhow::Context;
use rusqlite::Transaction;

use super::Migration;

pub struct Migration0044 {}

impl Migration for Migration0044 {
	fn name(&self) -> &str { "Persist the mailbox payout wallet" }
	fn to_version(&self) -> i64 { 44 }
	fn do_migration(&self, tx: &Transaction) -> anyhow::Result<()> {
		tx.execute_batch("CREATE TABLE bark_mailbox_bdk (
			id INTEGER PRIMARY KEY CHECK (id = 1),
			changeset TEXT NOT NULL
		);")
			.context("create mailbox payout wallet state")?;
		Ok(())
	}
}
