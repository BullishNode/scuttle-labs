-- A terminal Failed attempt cannot authorize another payment by replaying
-- the owner's old request. Older attempts remain readable with NULL IDs.
ALTER TABLE lightning_payment_attempt
	ADD COLUMN intent_id TEXT UNIQUE CHECK (intent_id IS NULL OR length(intent_id) = 64);
