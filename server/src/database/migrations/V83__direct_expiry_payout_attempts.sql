-- Preserve every fee replacement. A prior attempt can confirm after a newer
-- attempt was stored; the allocation then belongs to the confirmed attempt.
CREATE TABLE expiry_payout_attempt (
	allocation_id BIGINT NOT NULL REFERENCES expiry_allocation(id),
	txid TEXT NOT NULL CHECK (length(txid) = 64),
	signed_tx BYTEA NOT NULL,
	fee_sat BIGINT NOT NULL CHECK (fee_sat >= 0),
	created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
	PRIMARY KEY (allocation_id, txid),
	UNIQUE (txid)
);

-- Existing recovery databases retain their zero-fee version-3 parents and
-- historical split requests. They must use the old recovery stack to publish.
INSERT INTO expiry_payout_attempt(allocation_id,txid,signed_tx,fee_sat)
SELECT id,sweep_txid,sweep_tx,0 FROM expiry_allocation WHERE sweep_txid IS NOT NULL;

CREATE OR REPLACE VIEW expiry_allocation_state AS
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
	CASE WHEN o.confirmed THEN o.confirmed_txid ELSE a.sweep_txid END AS sweep_txid,
	CASE WHEN o.confirmed THEN winner.signed_tx ELSE a.sweep_tx END AS sweep_tx,
	a.broadcast_at,a.created_at,
	CASE WHEN o.confirmed THEN 'confirmed'
		WHEN o.conflicted OR EXISTS (
			SELECT 1 FROM expiry_allocation_coin c WHERE c.allocation_id=a.id AND (
				EXISTS(SELECT 1 FROM expiry_terminal_claim t WHERE t.entitlement_id=c.entitlement_id)
				OR EXISTS(SELECT 1 FROM expiry_allocation_coin paid JOIN observed prior ON prior.id=paid.allocation_id
					WHERE paid.entitlement_id=c.entitlement_id AND prior.confirmed AND prior.id<>a.id)
				OR (c.evidence->>'kind'='htlc_internal_grants_reclaimed' AND NOT EXISTS(
					SELECT 1 FROM public.watchmand_block b WHERE b.height=(c.evidence->>'height')::BIGINT
						AND b.hash=c.evidence->>'hash'))))
		THEN 'conflicted' ELSE 'pending' END AS status,
	CASE WHEN o.confirmed THEN o.height END AS confirmed_height,
	CASE WHEN o.confirmed THEN b.hash END AS confirmed_hash
FROM expiry_allocation a JOIN observed o ON o.id=a.id
LEFT JOIN expiry_payout_attempt winner ON winner.allocation_id=a.id AND winner.txid=o.confirmed_txid
LEFT JOIN public.watchmand_block b ON b.height=o.height;
