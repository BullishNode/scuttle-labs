# Ownership and expiry payouts

Expiry payouts use protocol version 6. A payout server accepts only that
version. A server with payouts disabled accepts upstream versions 4 through 5.
The mode is fixed when its database is created. Server and wallet schema
changes use ordinary migrations.

## Owner authorization

Each customer VTXO on a payout server has an owner fixed at birth. Address type
`0x03` carries two signatures: one from the spending key and one from the
mailbox key. Both authorize the same intent, bound to its version, network,
server, spending key and blinded mailbox point. The server verifies both
signatures and retains the first binding. The same spending key cannot be
rebound to another mailbox.

A sender copies the recipient's proof for recipient outputs and supplies its
own proof for change. The submitting wallet does not determine the output's
owner. Recovery hints do not create ownership. Mailbox delivery must match the
verified recipient binding.

The economic operation selects the entitled spending key before the server
resolves its mailbox key `M`. Ordinary unspent coins use their birth owner.
An unfinished transfer can owe its input owner; a collected forwarded receive
owes its admitted destination. Uncertain obligations stay held.

The destination is BIP86 `tr(M)`, with no script tree. It is derived from `M`,
not registered as a separate bitcoin address. The owner proof does not identify
a person, require a common seed or authorize a bitcoin input. The
default Bark wallet derives its mailbox from BIP86 account zero at external index
zero. An application that supplies another mailbox key must back up that private key
and use its matching BIP86 derivation, discovery and spending account. The stored
public mailbox identity cannot restore an external private key. An unrelated
external mailbox cannot be recovered from the Ark seed alone.

## Status and wallet state

The server reports a committed right as pending until all relevant portions
are terminal. On-chain discharge requires six canonical confirmations. A
fully covered right with a paid portion reports `Spent`; an entirely forfeited
right reports `Forfeited`. Partial coverage remains pending.

Bark retires authenticated fallback commitments to ordinary `Spent` immediately.
An existing emergency exit keeps its `Exited` state. Optional `fallback_status`
is the last observed display, not a separate claim or balance. A later reorg
can change that display to pending but does not restore Ark spendability.
Ark consumption therefore does not by itself prove receipt of bitcoin.

The client trusts authenticated server outcomes for Ark state. It does not
maintain a payout receipt ledger or reconstruct entitlement finality from
bitcoin outputs. The ordinary bitcoin wallet handles output discovery,
confirmations, spending and reorgs. An unrelated deposit creates no Ark credit
and establishes no Lightning result.

Coin status and recovery use ownership authorization. Public body lookups do
not expose mailbox keys. Hash-only receive status also omits the mailbox key.
A receive outcome must match the original invoice and complete grant set;
a forwarding provider must not credit itself with its recipient's payment.
Missing, foreign or unanswered status does not authorize a refund.

## Lightning continuity

Only the original initiating task calls xpay. An exact retry or monitor does
not dispatch another payment. A live original zero-part reply with code 205,
206 or 207 can establish failure only after the existing native identity,
source and same-hash checks exclude successful, pending or unknown work.
A stored error or empty lookup cannot recreate that authority.

An ambiguous outcome stays held. Operator resolution uses the private
administrative interface and evidence about the same persistent native
backend. A cold-restart assertion means the original daemon and every plugin
have stopped and been reaped, the same persistent backend has restarted, and
dispatch was excluded during maintenance. The RPC does not stop Core Lightning
or prove that assertion. Native success uses the normal preimage settlement
path. Use unmodified Core Lightning with matching plugins; no Scuttle xpay patch
or slow-mode setting is required.
