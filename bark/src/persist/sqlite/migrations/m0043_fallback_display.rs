use anyhow::Context;
use rusqlite::Transaction;

use super::Migration;

pub struct Migration0043 {}

impl Migration for Migration0043 {
	fn name(&self) -> &str { "Store fallback display and the wallet mailbox identity" }
	fn to_version(&self) -> i64 { 43 }
	fn do_migration(&self, tx: &Transaction) -> anyhow::Result<()> {
		tx.execute_batch("ALTER TABLE bark_vtxo ADD COLUMN fallback_status TEXT
			CHECK (fallback_status IN ('pending','spent','forfeited'));
			ALTER TABLE bark_properties ADD COLUMN mailbox_pubkey TEXT CHECK (length(mailbox_pubkey)=66);
			ALTER TABLE bark_properties ADD COLUMN server_payouts INTEGER DEFAULT 0 CHECK (server_payouts IN (0,1));")
			.context("add fallback display and mailbox property")?;
		Ok(())
	}
}
