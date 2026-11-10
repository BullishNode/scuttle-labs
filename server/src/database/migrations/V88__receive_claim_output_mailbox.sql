-- A settled receive claim fixes the final output's payout mailbox before
-- the server signs or the client registers that output.
ALTER TABLE expiry_lightning_receive_claim
	ADD COLUMN output_mailbox TEXT CHECK (output_mailbox IS NULL OR length(output_mailbox) = 66);
