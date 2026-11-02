# Expiry payouts

An Ark server can pay an expired customer VTXO to its owner instead of the
operator. The server withholds a payout while its owner or payment outcome is
unresolved.

## Enable payouts

Set `expiry_payouts` before the server database is created. The default is
`false`. The database records this choice and refuses a different value on
reopen. With payouts enabled, the server requires protocol version 6 and a
verified owner for every customer VTXO. With payouts disabled, it accepts the
ordinary upstream protocol range.

Run captaind and watchmand against the same database. Keep the watchman fee
wallet funded. Use the ordinary server and wallet migration runners.

## Destination and fees

The server selects the entitled owner from the coin's current operation. An
unfinished transfer can still owe its input owner. A collected forwarded
receive owes its committed recipient, even if the recipient output is absent.
The payout uses that owner's mailbox key to derive a BIP86 Taproot output.

Customer amounts follow the payout fee and dust policy. A dust-forfeited amount
has no customer output. The operator's fee wallet sponsors miner fees through a
separate child transaction. The payout parent remains unchanged when its fee
child changes. A lack of usable fee-wallet bitcoin delays publication.

## Payment outcomes

An uncertain Lightning payment is not a refund. Missing native records,
connection errors and elapsed time do not establish failure. Only the original
payment task dispatches xpay; monitors and retries observe that recorded call.
Use the private administrative operation only with evidence that the original
backend has no remaining successful, pending or unknown work. Do not replace
its store or node to make an unresolved payment disappear.

The server uses six canonical confirmations for an on-chain terminal outcome.
The client treats a fallback commitment as ordinary `Spent`, including while its
payout is pending. Its optional fallback display does not add Ark or bitcoin
balance. The bitcoin wallet discovers and spends actual on-chain outputs.
See [ownership and client behavior](scuttle-protocol.md) for recovery limits.
