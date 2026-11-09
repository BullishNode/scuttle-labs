-- A late small entitlement can require an operator top-up after the owner
-- already paid the scheduled charge in another confirmed allocation.
ALTER TABLE expiry_allocation_coin
	ADD COLUMN subsidy_sat BIGINT NOT NULL DEFAULT 0 CHECK (subsidy_sat >= 0);

ALTER TABLE expiry_allocation_coin
	DROP CONSTRAINT expiry_allocation_coin_check;

ALTER TABLE expiry_allocation_coin
	ADD CONSTRAINT expiry_allocation_coin_check CHECK (
		(disposition='customer' AND owner_mailbox IS NOT NULL
			AND user_pubkey IS NOT NULL AND spk IS NOT NULL
			AND gross_sat + subsidy_sat = fee_sat + payout_sat + forfeited_sat)
		OR (disposition='operator' AND fee_sat=0 AND payout_sat=0
			AND forfeited_sat=0 AND subsidy_sat=0)
	);
