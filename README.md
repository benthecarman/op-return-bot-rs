# OP_RETURN Bot

Rust implementation of OP_RETURN Bot.

The service creates paid Bitcoin OP_RETURN transactions. It supports Lightning
and unified Lightning/on-chain payment requests, NIP-05, LNURL, Nostr zaps,
Twitter, Telegram, MCP, and the existing SQLite database.

The Rust service keeps the deployed routes, HTML flow, transaction shape,
pricing rules, wallet split, and legacy SQLite encodings. It supports Bitcoin
mainnet and regtest. Lightning can use LND or ldk-server.

## Development

Copy `config.example.toml` to `op-return-bot.toml`, then run:

```shell
cargo run -- --config op-return-bot.toml
```

Run the local checks with:

```shell
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test bitcoin_rpc::tests -- --ignored
```

The ignored tests start a fake Bitcoin Core RPC server on a local TCP port.

The production database is not stored in Git. Set `ORB_PRODUCTION_DB` to a
snapshot path when you run the ignored compatibility test.

```shell
ORB_PRODUCTION_DB=/path/to/invoices.sqlite \
  cargo test --test database_compatibility \
  migrates_a_production_snapshot_without_losing_rows -- --ignored --exact
```

Build and test the Nix package with:

```shell
nix flake check
nix build .#
```

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for the staged rollout, database
backup, wallet notification setup, checks, and rollback procedure.
See [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) for the preserved production
contract and the intentional bug fixes.
