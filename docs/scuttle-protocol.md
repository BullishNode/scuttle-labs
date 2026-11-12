# Scuttle expiry payouts

`expiry_payouts` is fixed when the server database is created. A payout server
advertises `scuttle-1` and requires numeric protocol version 5. The label is
the payout-mode gate; version 5 alone does not identify a Scuttle server. With
payouts disabled, the server retains the upstream protocol range.

## Owner binding

Each customer VTXO gets an immutable `coin_owner(vtxo_id, M)` row in its birth
transaction. Board, round and default Lightning receive requests already carry
the mailbox key. For each Arkoor output, including change, the payer sends the
blinded mailbox point `B` from an upstream-format Ark address. The server
unblinds `B` with that output's spending key `P` and its own mailbox key to
recover `M`. It rejects a missing or invalid `B` before cosigning. Mailbox
delivery and authenticated coin status must match the recorded `M`.

The sender of an external payment chooses the recipient address. An altered,
valid `B` can bind the coin to a different mailbox while leaving `P` unchanged;
address-only admission does not prove that one seed controls both keys. A
recipient wallet credits only the signed coin after authenticated status
attests its own `owner_mailbox`. Missing delivery or status is not acceptance.

## Payout and status

The server decides the economic owner before choosing a payout mailbox. It
holds an unresolved node until its obligations resolve. A settled but unclaimed
forwarded Lightning grant still belongs to its provider; a recorded final
claim can bind its recipient output before that coin is registered.

A resolved root is spent in one version-2 transaction with owner outputs and
an operator output for operator-owned value only. Customer outputs pay
`tr(M)`, the BIP86 Taproot key-path output for the mailbox key. The default
Bark mailbox key comes from `m/350'/1'` on the Ark seed. The payout fee is the
higher of 2,000 sat or 1%, charged once for each owner in each expired tree;
the operator may add dust subsidy. An entitlement
whose fee leaves no standard output is reported as `Forfeited` with its gross,
fee and payout amounts. Miner fees are paid inline, and replacements keep the
owner outputs fixed.

A decided coin's status is the upstream `Spent` state plus a `fallback_payout`
field: `Pending` until the payout is six blocks deep, then `Paid`, or
`Forfeited` when the fee consumed the coin. The server reports it only to the
mailbox the coin was born with, and not for a coin its holder put on chain.
These are operator reports, not proof of Bitcoin receipt. Bark keeps the last
observed fallback status separately from Ark spendability. A Bitcoin wallet
must independently find and confirm the exact `tr(M)` output before counting
it as received.

## Lightning uncertainty

A preimage alone does not establish which send source paid. Failed sources with
unexplained settlement evidence remain held. Claims, revocations and payout
decisions use source-specific records and the decided-coin fence. Operators
must investigate a hold rather than infer failure from elapsed time or an
empty Lightning lookup.

## Invoices paid by arkoor

With `ln_receive_ark_address_hints` on the server and
`lightning_ark_address_hints` in the wallet, an invoice carries the receiver's
Ark address in two route hints. A compatible payer on the same server pays it
by arkoor through `RequestLightningReceiveArkoorCosign`. The server settles the
receive in the same write and the invoice can no longer be paid. On a payout
server the arkoor must bind its output to the mailbox the receive was started
for, so an expiry payout of that coin reaches the receiver.
