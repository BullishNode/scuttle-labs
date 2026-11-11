//! Read and lock the published watchman tip.

use bitcoin_ext::BlockRef;
use super::{BlockTable, Tx};

impl Tx<'_> {
	pub async fn expiry_view(&self) -> anyhow::Result<Option<BlockRef>> {
		self.get_highest_block(BlockTable::Watchmand).await
	}

	pub async fn lock_watchman_tip(&self, point: BlockRef) -> anyhow::Result<()> {
		// The birthday row survives every recoverable reorg. Use that stable
		// parent, not the moving maximum row, to order source decisions.
		self.query("SELECT height FROM watchmand_block ORDER BY height ASC LIMIT 1 FOR SHARE", &[]).await?;
		ensure!(self.get_highest_block(BlockTable::Watchmand).await? == Some(point), "published watchman tip changed");
		Ok(())
	}
}
