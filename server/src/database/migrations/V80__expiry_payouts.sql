SET LOCAL search_path TO public;

-- Existing upstream stores remain payout-off. Only empty-database creation
-- may choose the payout-on mode before issuing any coins.
CREATE TABLE server_mode (
	singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
	expiry_payouts BOOLEAN NOT NULL DEFAULT FALSE
);
INSERT INTO server_mode(singleton, expiry_payouts)
VALUES (TRUE, COALESCE(current_setting('ark.expiry_payouts', TRUE), 'false')::BOOLEAN);

-- The owner's signed send intent. A request sent again finds its attempt
-- by this id and cannot start another payment.
ALTER TABLE lightning_payment_attempt
	ADD COLUMN intent_id TEXT UNIQUE CHECK (intent_id IS NULL OR length(intent_id) = 64);

-- The admitted board fallback destination is fixed with the exit cosignature.
ALTER TABLE board_exit
	ADD COLUMN owner_mailbox TEXT
	CHECK (owner_mailbox IS NULL OR length(owner_mailbox) = 66);
ALTER TABLE board_exit
	ADD COLUMN user_pubkey TEXT
	CHECK (user_pubkey IS NULL OR length(user_pubkey) = 66);
CREATE INDEX board_exit_exit_txid_idx ON board_exit(exit_txid);

CREATE TABLE expiry_entitlement (
	id BIGSERIAL PRIMARY KEY,
	vtxo_id TEXT NOT NULL UNIQUE REFERENCES vtxo(vtxo_id),
	created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE expiry_cancelled_participation (
	-- Upstream may delete a superseded pending participation. Keep its cancellation tombstone.
	participation_id BIGINT PRIMARY KEY,
	unlock_hash TEXT UNIQUE,
	created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Financial decisions are immutable; frontier spends determine their status.
CREATE TABLE expiry_allocation (
	id BIGSERIAL PRIMARY KEY,
	expiry_height INTEGER NOT NULL CHECK (expiry_height >= 0),
	broadcast_at TIMESTAMPTZ,
	created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE expiry_allocation_input (
	allocation_id BIGINT NOT NULL REFERENCES expiry_allocation(id),
	vtxo_id TEXT NOT NULL REFERENCES vtxo(vtxo_id),
	PRIMARY KEY (allocation_id, vtxo_id)
);

-- All replacement attempts remain auditable; a prior attempt may confirm
-- after a later fee bump is stored.
CREATE TABLE expiry_payout_attempt (
	allocation_id BIGINT NOT NULL REFERENCES expiry_allocation(id),
	txid TEXT NOT NULL CHECK (length(txid) = 64),
	signed_tx BYTEA NOT NULL,
	fee_sat BIGINT NOT NULL CHECK (fee_sat >= 0),
	created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
	PRIMARY KEY (allocation_id, txid),
	UNIQUE (txid)
);

CREATE TABLE expiry_allocation_coin (
	allocation_id BIGINT NOT NULL REFERENCES expiry_allocation(id),
	entitlement_id BIGINT NOT NULL REFERENCES expiry_entitlement(id),
	owner_mailbox TEXT CHECK (owner_mailbox IS NULL OR length(owner_mailbox) = 66),
	user_pubkey TEXT,
	spk TEXT,
	disposition TEXT NOT NULL CHECK (disposition IN ('customer', 'operator')),
	gross_sat BIGINT NOT NULL CHECK (gross_sat > 0),
	fee_sat BIGINT NOT NULL CHECK (fee_sat >= 0),
	payout_sat BIGINT NOT NULL CHECK (payout_sat >= 0),
	forfeited_sat BIGINT NOT NULL CHECK (forfeited_sat >= 0),
	subsidy_sat BIGINT NOT NULL DEFAULT 0 CHECK (subsidy_sat >= 0),
	evidence JSONB NOT NULL,
	PRIMARY KEY (allocation_id, entitlement_id),
	CHECK ((disposition = 'customer' AND owner_mailbox IS NOT NULL AND user_pubkey IS NOT NULL
		AND spk IS NOT NULL AND gross_sat + subsidy_sat = fee_sat + payout_sat + forfeited_sat)
		OR (disposition = 'operator' AND fee_sat = 0 AND payout_sat = 0
			AND forfeited_sat = 0 AND subsidy_sat = 0))
);

-- Canonical frontier spends are the source of allocation state. The first
-- coin row retains ownership; a winner also closes competing allocation links.
CREATE VIEW expiry_allocation_state AS
WITH observed AS (
	SELECT a.id,
		COUNT(i.vtxo_id)>0
			AND BOOL_AND(v.onchain_spent_txid IS NOT NULL AND b.height IS NOT NULL
				AND EXISTS(SELECT 1 FROM expiry_payout_attempt p
					WHERE p.allocation_id=a.id AND p.txid=v.onchain_spent_txid))
			AND COUNT(DISTINCT v.onchain_spent_txid)=1 AS confirmed,
		BOOL_OR(v.onchain_spent_txid IS NOT NULL AND NOT EXISTS(
			SELECT 1 FROM expiry_payout_attempt p
			WHERE p.allocation_id=a.id AND p.txid=v.onchain_spent_txid))
			OR COUNT(DISTINCT v.onchain_spent_txid)>1 AS conflicted,
		MIN(v.onchain_spent_txid) AS confirmed_txid,
		MIN(b.height) AS height
	FROM expiry_allocation a
	LEFT JOIN expiry_allocation_input i ON i.allocation_id=a.id
	LEFT JOIN public.vtxo v ON v.vtxo_id=i.vtxo_id
	LEFT JOIN public.watchmand_block b ON b.height=v.onchain_spent_height
	GROUP BY a.id
)
SELECT a.id,a.expiry_height,
	CASE WHEN o.confirmed THEN o.confirmed_txid ELSE latest.txid END AS sweep_txid,
	CASE WHEN o.confirmed THEN winner.signed_tx ELSE latest.signed_tx END AS sweep_tx,
	a.broadcast_at,a.created_at,
	CASE WHEN o.confirmed THEN 'confirmed'
		WHEN o.conflicted OR EXISTS (
			SELECT 1 FROM expiry_allocation_coin c JOIN expiry_allocation_coin paid ON paid.entitlement_id=c.entitlement_id
			JOIN observed prior ON prior.id=paid.allocation_id
			WHERE c.allocation_id=a.id AND prior.confirmed AND prior.id<>a.id)
		THEN 'conflicted' ELSE 'pending' END AS status,
	CASE WHEN o.confirmed THEN o.height END AS confirmed_height,
	CASE WHEN o.confirmed THEN b.hash END AS confirmed_hash
FROM expiry_allocation a JOIN observed o ON o.id=a.id
LEFT JOIN expiry_payout_attempt winner ON winner.allocation_id=a.id AND winner.txid=o.confirmed_txid
-- Each stored replacement raises the absolute fee, so the costliest attempt is the latest.
LEFT JOIN LATERAL (SELECT p.txid,p.signed_tx FROM expiry_payout_attempt p
	WHERE p.allocation_id=a.id ORDER BY p.fee_sat DESC LIMIT 1) latest ON TRUE
LEFT JOIN public.watchmand_block b ON b.height=o.height;

CREATE TABLE coin_owner (
	vtxo_id TEXT PRIMARY KEY REFERENCES public.vtxo(vtxo_id),
	mailbox_pk TEXT NOT NULL CHECK (length(mailbox_pk) = 66)
);
