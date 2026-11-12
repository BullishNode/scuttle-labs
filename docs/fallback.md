# Expiry payout operations

Set `expiry_payouts` before creating the server database. Run captaind and
watchmand against that database and fund the operator fee wallet. A payout
server advertises `scuttle-1` and numeric protocol version 5. Existing payout-off
databases remain in upstream mode.

Watchmand leaves a node containing an unresolved obligation untouched. Once
the node is resolved, it spends the expired root in one transaction to its
recorded customer mailboxes and to the operator for operator-owned value.
The transaction pays its miner fee inline and can be replaced by fee with
fixed owner outputs. The customer charge is the higher of 2,000 sat or 1%.
An output too small after that charge is reported with zero payout and a
`Forfeited` outcome; the operator can subsidize dust where the policy allows.

Every customer payout output is `tr(M)`, where `M` is the admitted mailbox
key. The default Bark seed derives `M` at `m/350'/1'`. A separate Bitcoin
wallet must watch that Taproot script, confirm the actual outpoint and spend
it. Server payout status alone does not prove Bitcoin receipt. See
[the protocol notes](scuttle-protocol.md) for binding and status limits.
