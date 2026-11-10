-- Keep the admitted fallback destination with the board exit cosignature.
ALTER TABLE board_exit
	ADD COLUMN owner_mailbox TEXT
	CHECK (owner_mailbox IS NULL OR length(owner_mailbox) = 66);
