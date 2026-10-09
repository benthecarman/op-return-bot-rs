# OP_RETURN Bot

Rust implementation of OP_RETURN Bot.

The service creates paid Bitcoin OP_RETURN transactions. It supports Lightning
and unified Lightning/on-chain payment requests, NIP-05, LNURL, Nostr zaps,
Twitter, Telegram, MCP, and the existing SQLite database.

The Rust service keeps the deployed routes, HTML flow, transaction shape,
pricing rules, wallet split, and legacy SQLite encodings. It supports Bitcoin
mainnet and regtest. Lightning payments go through ldk-server, with BOLT11
invoices and BOLT12 offers.

## ldk-server macaroon

By default, the ldk-server backend uses ldk-server's admin macaroon. To give
the bot only the access it needs, create a macaroon for it on the ldk-server
host:

```shell
ldk-server-cli create-macaroon op-return-bot \
  --permissions invoices:create node:read payments:read events:read \
  | jq -r .token > ldk-server.macaroon
```

Then set `macaroon_file` in `[lightning.ldk_server]` to the path of that
file. On NixOS, load the file with `services.op-return-bot.credentials` and
use the path under `/run/credentials/op-return-bot.service/`.

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

See [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) for the preserved production
contract and the intentional bug fixes.
