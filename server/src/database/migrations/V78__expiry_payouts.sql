SET LOCAL search_path TO public;

-- Existing upstream stores remain payout-off. Only empty-database creation
-- may choose the payout-on mode before issuing any coins.
CREATE TABLE server_mode (
	singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
	expiry_payouts BOOLEAN NOT NULL DEFAULT FALSE
);
INSERT INTO server_mode(singleton, expiry_payouts)
VALUES (TRUE, COALESCE(current_setting('ark.expiry_payouts', TRUE), 'false')::BOOLEAN);

-- Keep exact invoice identity for retries of an already recorded initiation.
ALTER TABLE public.cln_xpay ADD COLUMN signed_invoice TEXT;

CREATE INDEX expiry_vtxo_anchor_point_ix ON vtxo (anchor_point, vtxo_id);

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
	sweep_txid TEXT UNIQUE,
	sweep_tx BYTEA,
	broadcast_at TIMESTAMPTZ,
	created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
	CHECK ((sweep_txid IS NULL) = (sweep_tx IS NULL))
);

CREATE TABLE expiry_allocation_input (
	allocation_id BIGINT NOT NULL REFERENCES expiry_allocation(id),
	vtxo_id TEXT NOT NULL REFERENCES vtxo(vtxo_id),
	PRIMARY KEY (allocation_id, vtxo_id)
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
	evidence JSONB NOT NULL,
	PRIMARY KEY (allocation_id, entitlement_id),
	CHECK ((disposition = 'customer' AND owner_mailbox IS NOT NULL AND user_pubkey IS NOT NULL
		AND spk IS NOT NULL AND gross_sat = fee_sat + payout_sat + forfeited_sat)
		OR (disposition = 'operator' AND fee_sat = 0 AND payout_sat = 0 AND forfeited_sat = 0))
);
-- Splitting is a chain action, not another payout decision or customer fee.
CREATE TABLE expiry_split_request (
	vtxo_id TEXT PRIMARY KEY REFERENCES vtxo(vtxo_id),
	progress_txid TEXT NOT NULL,
	created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE expiry_terminal_claim (
	entitlement_id BIGINT PRIMARY KEY REFERENCES expiry_entitlement(id),
	txid TEXT NOT NULL,
	height BIGINT NOT NULL CHECK (height >= 0),
	hash TEXT NOT NULL
);

-- Canonical frontier spends are the source of allocation state. The first
-- coin row retains ownership; a winner also closes competing allocation links.
CREATE VIEW expiry_allocation_state AS
WITH observed AS (
	SELECT a.id,
		COUNT(i.vtxo_id)>0 AND BOOL_AND(COALESCE(v.onchain_spent_txid=a.sweep_txid, FALSE)
			AND b.height IS NOT NULL) AS confirmed,
		BOOL_OR(v.onchain_spent_txid IS NOT NULL
			AND v.onchain_spent_txid IS DISTINCT FROM a.sweep_txid) AS conflicted,
		MIN(b.height) AS height
	FROM expiry_allocation a
	LEFT JOIN expiry_allocation_input i ON i.allocation_id=a.id
	LEFT JOIN public.vtxo v ON v.vtxo_id=i.vtxo_id
	LEFT JOIN public.watchmand_block b ON b.height=v.onchain_spent_height
	GROUP BY a.id
)
SELECT a.id,a.expiry_height,a.sweep_txid,a.sweep_tx,a.broadcast_at,a.created_at,
	CASE WHEN o.confirmed THEN 'confirmed'
		WHEN o.conflicted OR EXISTS (
			SELECT 1 FROM expiry_allocation_coin c WHERE c.allocation_id=a.id AND (
				EXISTS(SELECT 1 FROM expiry_terminal_claim t WHERE t.entitlement_id=c.entitlement_id)
				OR EXISTS(SELECT 1 FROM expiry_allocation_coin paid JOIN observed winner ON winner.id=paid.allocation_id
					WHERE paid.entitlement_id=c.entitlement_id AND winner.confirmed AND winner.id<>a.id)
				OR (c.evidence->>'kind'='htlc_internal_grants_reclaimed' AND NOT EXISTS(
					SELECT 1 FROM public.watchmand_block b WHERE b.height=(c.evidence->>'height')::BIGINT
						AND b.hash=c.evidence->>'hash'))))
		THEN 'conflicted' ELSE 'pending' END AS status,
	CASE WHEN o.confirmed THEN o.height END AS confirmed_height,
	CASE WHEN o.confirmed THEN b.hash END AS confirmed_hash
FROM expiry_allocation a JOIN observed o ON o.id=a.id
LEFT JOIN public.watchmand_block b ON b.height=o.height;

-- A collected forwarding receive names its final payee independently of the
-- provider's grant key. Outcome remains the upstream subscription's status.
CREATE TABLE expiry_lightning_receive_claim (
	source_vtxo_id TEXT PRIMARY KEY REFERENCES vtxo(vtxo_id),
	subscription_id BIGINT NOT NULL REFERENCES lightning_htlc_subscription(id),
	payment_hash TEXT NOT NULL,
	output_vtxo_id TEXT NOT NULL UNIQUE,
	output_vtxo BYTEA NOT NULL
);

-- Both signatures are checked once at admission; preserve the first binding.
CREATE TABLE owner_key (
	user_pubkey TEXT PRIMARY KEY CHECK (length(user_pubkey) = 66),
	mailbox_pk TEXT NOT NULL CHECK (length(mailbox_pk) = 66),
	blinded_id TEXT NOT NULL CHECK (length(blinded_id) = 66),
	joint_proof TEXT NOT NULL CHECK (length(joint_proof) = 322)
);

CREATE TABLE coin_owner (
	vtxo_id TEXT PRIMARY KEY REFERENCES public.vtxo(vtxo_id),
	mailbox_pk TEXT NOT NULL CHECK (length(mailbox_pk) = 66)
);

-- Check at commit so the writer can insert the VTXO before its paired row.
CREATE FUNCTION require_customer_coin_owner() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
	IF (SELECT expiry_payouts FROM server_mode WHERE singleton)
		AND NEW.spend_state IN ('spendable', 'unregistered', 'unclaimed', 'htlc-recv-unclaimed')
		AND NOT (NEW.spend_state = 'unclaimed' AND NEW.policy =
			decode('0002dfa52f6690299d2d6a08323083e290597b56fee125063e5f4e2957731639c42c', 'hex'))
		AND NOT EXISTS (SELECT 1 FROM coin_owner WHERE vtxo_id=NEW.vtxo_id)
	THEN
		RAISE EXCEPTION 'customer VTXO has no coin_owner' USING ERRCODE='23514';
	END IF;
	RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER customer_coin_owner AFTER INSERT ON public.vtxo
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_customer_coin_owner();
