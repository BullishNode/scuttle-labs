# Pinned Rust SDK consumer

This standalone Cargo project depends on the pushed Scuttle Git revision in
`Cargo.toml`. It is intentionally outside the Scuttle workspace, so its build
checks the public Git dependency instead of a local path dependency. Update
the pin when validating a new candidate commit.

Build in the Scuttle Nix shell with `cargo build --manifest-path
testing/external-consumer-smoke/Cargo.toml`. Set `SCUTTLE_CONSUMER_EXE` to the
resulting binary and run the ignored
`payouts::wire::external_git_consumer_sends_and_receives_with_paired_payout_server`
integration test. That test opens the external wallet against the paired
server, sends a payment into it, syncs its mailbox and fallback status, then
sends a payment back through the pinned SDK.
