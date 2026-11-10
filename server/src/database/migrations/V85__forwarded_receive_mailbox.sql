-- The notification mailbox belongs to the invoice provider. Keep the
-- explicit recipient's payout mailbox separately from it.
ALTER TABLE lightning_htlc_subscription
	ADD COLUMN claim_destination_mailbox_pk TEXT
	CHECK (claim_destination_mailbox_pk IS NULL OR length(claim_destination_mailbox_pk) = 66);
