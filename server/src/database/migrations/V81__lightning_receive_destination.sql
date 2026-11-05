-- Preserve the checked recipient for the lifetime of the invoice.
ALTER TABLE lightning_htlc_subscription
	ADD COLUMN claim_destination_user_pubkey TEXT
	CHECK (claim_destination_user_pubkey IS NULL OR length(claim_destination_user_pubkey) = 66);
