# Scuttle ownership and fallback

Scuttle identifies a customer coin by its spending public key and an independently
authorized mailbox. The spending key holder and mailbox key holder both sign the
same server- and network-bound owner intent. A sender copies that proof from the
recipient's address when creating an output for them; it supplies its own proof
for change. A request's submitting wallet does not determine the output's owner.

The registration programme is removed. Fresh matched server and wallet stores
use one atomic Scuttle initializer alongside the unchanged upstream migration
history. Registration-shaped or incompatible stores are refused before writes;
keep their original software and data.

## Wire contract

The receipt-free Scuttle protocol epoch is `1001`. Epoch `1000` is refused. Clients negotiate it through `Handshake`;
all other Ark and Mailbox methods except the bootstrap information request require
the negotiated version. This includes recovery, status, streams and retry paths.
The operator service uses the configured private administrative listener, never
the public Ark or Mailbox routes. Its network trust boundary is unchanged.

`captaind rpc --addr <private-admin-address> reconcile-lightning-attempt <attempt-id>`
observes an existing external payment on its recorded online node. It returns
the current attempt state; it cannot choose an outcome, switch nodes or dispatch
another payment. Pending, missing and unknown outcomes remain protected. Restore
the same original node and store before reconciling an orphaned attempt. A manual
check, an elapsed deadline or an empty node response never authorizes a refund.

New functional fork fields use tags `1000` and above. Upstream invoice
`expiry_secs` retains tag `6`. The four retired owner-proof slots (board `7`,
VTXO request `3`, arkoor destination `3`, receive-claim preparation `6`) are
decode-only optional-byte tombstones. Any presence, including an empty value or
a mixture with a valid current proof, is rejected before effects. Those numbers
are permanently unavailable for functional reuse; unrelated reserved tags stay
reserved. Address type `0x03` carries the joint
owner proof. The proof commits to its version, genesis hash, server mailbox key,
full compressed spending key and blinded mailbox point. The coin half uses
`Ark/ExpiryOwnerLink/Coin/v2`; the mailbox half uses
`Ark/ExpiryOwnerLink/Mailbox/v2`. Their signatures are not interchangeable.

Coin and mailbox points retain their full compressed parity. X-only conversion
is used for BIP340 verification and BIP341 Taproot construction, not to compare
persisted mailbox identities. No signature proves a common human, common seed,
or ownership of an unrelated Bitcoin spending key.

## Admission, birth and delivery

Admission verifies both signatures and stores an immutable `owner_key` binding.
The same spending key, mailbox and signed fields are an idempotent replay even
if a valid signature differs; the first verified proof is retained. A different
binding for that key is refused. Recovery hints neither establish nor change it.

Customer births are `Spendable`, `Unregistered`, `Unclaimed` and
`HtlcRecvUnclaimed`. The issuing transaction writes the verified key binding,
then the VTXO, then its `coin_owner` row. The deferred constraint checks the
paired row at commit. Operator pool, forfeits, connectors and the server's exact
unspendable round padding policy do not become customer claims. Knowing the
padding public point does not bypass the issuing signature verifier.

Every spendable-coin delivery checks the entire batch against its destination
mailbox before allocating a checkpoint, in the same insertion transaction. A
mixed or incomplete ownership batch is refused atomically. A valid repeated post remains idempotent. Recovery-ID posts are
hints only: they may name an existing foreign coin without assigning ownership
to the recovery mailbox. An unknown ID fails the existing foreign-key constraint;
a mixed known/unknown post rolls back atomically.

## Economic payee and payout script

Birth ownership does not replace the economic stage resolver. Ordinary coins
use their birth mailbox. An unregistered transfer may still owe the input owner;
a collected forwarded receive owes its committed claim destination, even if its
final output was not inserted. Round participation and per-source Lightning
finality retain their existing evidence rules. A provider's mailbox is not copied
onto a recipient's output. An unresolved obligation is held; time or a failed
RPC does not turn it into a refund or operator income.

After selecting the entitled spending key, the derived payout path resolves its
mailbox `M` and constructs BIP86 `tr(M)` with an empty script tree. BIP341 lifts
the x-coordinate to even Y before applying `TapTweak`. A default mnemonic wallet
uses its standard BIP86 account, external index zero; an application may supply
its mailbox keypair explicitly and is responsible for that key's backup and
discovery. The default test wallet derives its mailbox at `m/86'/coin'/0'/0/0`,
matching its native BIP86 external index-zero descriptor. The Ark spending-key
branch remains separate. Restoring an external mailbox requires its original
key, network, account, passphrase and discovery policy; the protocol does not
prove those backup choices.

Payout aggregation is per mailbox, expiry and allocation group; size limits can
create several groups. Existing cumulative fee and dust rules apply. A signature
over the mailbox authorization challenge cannot authorize a Bitcoin input:
mailbox authorization hashes its fixed prefix and expiry timestamp, while a
key-path Bitcoin spend uses the transaction-specific BIP341 `TapSighash` and the
Taproot-tweaked key.

## Client and restore contract

Supply the same external mailbox keypair at each open. The wallet persists its
public identity, full parity and network, and refuses a different identity before
network synchronization. Generate a joint link whenever deriving a spending key;
transport the recipient's link for recipient outputs and the wallet's own link
for board, change, self-round and HTLC-send outputs.

Authenticated recovery, coin status and mailbox delivery carry `owner_mailbox`.
Batch ownership entries bind the mailbox to its VTXO ID, independently of request
duplicates or response order. A missing or foreign entry establishes no wallet
ownership. Public `GetVtxo` and `GetVtxosByIds` return signed bodies without mailbox
metadata; knowing an ID must not disclose the stable mailbox key.

Coin status projects the existing economic resolver onto the original known
right. It aggregates every relevant input-owner alias and split portion.
`FALLBACK_PENDING` means at least one committed portion is unresolved, below six
canonical confirmations, or not yet completely covered. All relevant portions
must be terminal before retirement: any paid portion yields `Spent`; an entirely
no-output result yields `FALLBACK_FORFEITED`. The commitment bit distinguishes a
first-seen fallback-terminal coin from ordinary spent history. It is not a payout
proof or a statement that the birth mailbox is the stage-entitled Bitcoin payee.

A collected receive with no final coin uses its existing invoice/hash status.
The server binds the result to the actual subscription, complete original grant
set and admitted destination. The hash-only response returns only the narrow
outcome, never an unblinded mailbox key. The client retains its original claim
destination and grant identities on the existing operation record; a provider
must not adopt its recipient's positive Ark balance.

Clients trust authenticated server outcome for Ark retirement. They no longer
independently attribute or verify each entitlement's Bitcoin payout. There is no
public claim model, claim feed, allocation cursor or client receipt ledger.
Allocation IDs, source aliases, payout locators and per-leaf fee/principal splits
remain internal server accounting. Bitcoin discovery, amounts, confirmations,
spending and reorgs belong to the ordinary Bitcoin wallet. An unrelated deposit
must not retire an Ark coin or imply a Lightning result.

An undecided hold leaves the coin spendable, refreshable and exitable. A known
commitment is persisted before release and remains fenced across ordinary holder
cleanup, cancellation, status failures and reopen. Keep signed checkpoints until
the exact operation is terminal; a refused offboard cannot become successful.
Known fallback-terminal coin and receive identities are rechecked after reopen
and sync. When the server is available after a reorg, authenticated status restores
pending and keeps the fence, never spendability. While it is unavailable, the last
known Ark commitment stays fenced; the client does not independently reconstruct
Ark payout finality from Bitcoin outputs. Bitcoin sync still processes the output
reorg independently.
Unknown, missing, foreign or stale status cannot release a prior commitment.
Already exposed exits keep their normal observation and cleanup paths.

`fallback_status` on the existing coin or receive record is a status, not another
spendable balance or a synthetic Bitcoin credit. Committed rights are excluded
from ordinary Ark balance buckets. Original operation intent and its actual Ark
result remain in normal history; no per-entitlement synthetic movement is made.
The Bitcoin account counts each actual aggregated output once.

The default product wallet uses the same seed and passphrase for separate Ark
and BIP86 branches; its reserved account-zero external index-zero payout address
is deliberately reused. Restoring that Bitcoin account must support actual
account discovery, not just Taproot signing. An application-supplied external
mailbox key is a different backup contract: the application must back up that
signing key (or its separate seed/path), repeat the same identity at open, and
integrate the corresponding Bitcoin discovery and spending account. Restoring
only the Ark seed cannot recover an unrelated external mailbox key or its funds.
No seed relationship is enforced by the protocol. Seed recovery does not promise
discovery of every lost unfinished operation identifier; known operations remain
queryable, and actual Bitcoin payouts remain independently discoverable.

Release canceled-round inputs only after the authenticated cancellation for the
exact participation, and only if no forfeit could have been exposed. Persist the
cancellation before unlocking. An unknown round is not cancellation. A restore
re-derives or imports the backed-up mailbox key and applies the same ownership and
authenticated narrow-status rules.

The initial release uses fresh matched Scuttle stores. It retains upstream
migration payloads and runners unchanged; it does not upgrade funded registration-
era Scuttle stores or receipt-bearing wallet stores. Unrecognized, partial or incompatible
SQL stores are refused before writes instead of being silently repaired. Server reopen requires the
current Scuttle tables, the completed-view columns in the `scuttle` schema, and
the intact upstream history through 77. Wallet reopen requires its existing coin
outcome and receive-association columns and upstream history through 42. Old
registration, dual-ack and wallet receipt layouts are not upgraded. These are
matched fresh-store requirements, not a database repair or conversion service.


## Lightning dispatch continuity

An empty lookup for the attempt's own native label is not a failure receipt.
Only the original initiating task calls xpay. The monitor, exact client retries
and operator reconciliation observe existing work; none resubmits it. A task lost
before its first RPC leaves the recorded attempt unknown and held. An empty native
lookup or elapsed time cannot release it. A native command already running when
Bark restarts can still finish, and its own labelled outcome resolves normally.

The original call freshly bounds `maxdelay` by its original absolute HTLC deadline,
using the higher native and processed server height. A live original zero-part reply
(205, 206 or 207) may retire only that exact original call, with empty own native history and
no same-hash Pending or Complete group. The exact node, source, settlement and
hash guards remain. Stored errors and copied outcome rows cannot recreate this
live authority. Intra-Ark payments never invoke xpay.

Keep the recorded node's endpoint, authentication and persistent store continuous.
A public node key does not prove a store generation, and process-local dispatch
guards do not coordinate multiple server processes. Run one captaind dispatcher
per store. A stale or ambiguous PostgreSQL/CLN restore stays disabled until its
continuity is established; do not switch an unresolved attempt to another node.
Existing Scuttle stores with a resend counter are preserved and refused rather
than upgraded in place.

Stock CLN 26.06.6 does not require xpay slow mode. At a controlled cutover,
unset any persistent `xpay-slow-mode=true` setting written by an older Scuttle
version before starting the matched application. Removing the application setter
does not clear an existing native configuration. Native identity, network and
persistent-store continuity remain required.
