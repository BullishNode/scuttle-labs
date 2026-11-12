# Scuttle Run 4 Rust wallet

The `bark-wallet` package in this Scuttle repository is the Rust SDK for its
paired Ark server. A Rust application should pin a tested Scuttle Git commit:

```toml
bark-wallet = { git = "https://github.com/BullishNode/scuttle", rev = "<tested-commit>" }
```

The library is imported as `bark`. The published `bark-wallet` package is an
upstream release and does not select this Scuttle implementation.

Set `Config::require_scuttle_payouts = true` before `Wallet::open` for a Run 4
wallet. Opening then requires a reachable Ark server that advertises the
`scuttle-1` handshake label. With the option disabled, an ordinary upstream
server remains usable. The server and wallet must come from the same tested
Scuttle commit.

Ark addresses retain upstream bytes. For each Arkoor output the wallet sends
the blinded mailbox identifier from the recipient's address, or derives one
from its own output key and fixed mailbox key. The server unblinds that value
with the output's actual coin key to bind its payout mailbox at birth. The
blinded point does not prove that the coin key and mailbox key share a seed.
A receiving wallet must verify
the actual server-cosigned coin and its attested `owner_mailbox` before
crediting it. A missing delivery or status is not acceptance.

The server reports a decided coin as `Spent` with a fallback status of
`Pending`, `Paid` (stored as `Spent`), or `Forfeited`; it is
not proof that bitcoin arrived. The application's Bitcoin wallet must watch
and spend `tr(M)` separately. Bark does not contain a second payout Bitcoin
wallet or count a server-reported payout as received on-chain bitcoin.
