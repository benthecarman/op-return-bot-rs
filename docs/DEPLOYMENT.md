# Production deployment

This procedure replaces the Scala service with the Rust service. It keeps the
same SQLite data and public routes. Plan a one-hour maintenance window.

## Milestone 1: Prepare and verify

1. Build the exact Git revision that you will deploy:

   ```shell
   nix flake check
   nix build .#
   ```

2. Copy the production SQLite file to a test location. Do not test migrations
   on the active file.
3. Run the production migration test:

   ```shell
   ORB_PRODUCTION_DB=/path/to/snapshot.sqlite \
     cargo test --test database_compatibility \
     migrates_a_production_snapshot_without_losing_rows -- --ignored --exact
   ```

4. Create `op-return-bot.toml` from `config.example.toml`. Put each secret in a
   separate file. Set `lightning.backend` to `lnd` for the first deployment.
5. Test the full flow on regtest: create a web request, pay by Lightning, pay a
   unified request on-chain, restart the service before settlement, and confirm
   that reconciliation creates each OP_RETURN only once.

## Milestone 2: Install without traffic

Import the flake module and configure the service:

```nix
{
  imports = [ inputs.op-return-bot.nixosModules.default ];

  services.op-return-bot = {
    enable = true;
    configFile = /etc/op-return-bot.toml;
    credentials = {
      bitcoin-rpc-password = /run/secrets/bitcoin-rpc-password;
      wallet-notify-key = /run/secrets/wallet-notify-key;
      lnd-admin.macaroon = /run/secrets/lnd-admin.macaroon;
      lnd-tls.cert = /run/secrets/lnd-tls.cert;
      nostr-nsec = /run/secrets/nostr-nsec;
      twitter-consumer-key = /run/secrets/twitter-consumer-key;
      twitter-consumer-secret = /run/secrets/twitter-consumer-secret;
      twitter-access-token = /run/secrets/twitter-access-token;
      twitter-access-secret = /run/secrets/twitter-access-secret;
      telegram-token = /run/secrets/telegram-token;
    };
  };
}
```

Credential paths in the TOML file use
`/run/credentials/op-return-bot/<credential-name>`.

Start the Rust service on a private port with a copy of the database. Check
the home page, static assets, QR images, REST calls, MCP discovery,
NIP-05, and LNURL responses. Do not send production payments in this step.

## Milestone 3: Cut over

1. Stop new traffic at the reverse proxy.
2. Stop the Scala service. Confirm that it has exited.
3. Checkpoint SQLite and make a recoverable backup:

   ```shell
   sqlite3 /var/lib/op-return-bot/invoices.sqlite 'PRAGMA wal_checkpoint(TRUNCATE);'
   sqlite3 /var/lib/op-return-bot/invoices.sqlite \
     ".backup '/var/backups/op-return-bot/invoices-before-rust.sqlite'"
   ```

4. Start the Rust service against the active database. Startup applies the
   additive SQLx migrations in one transaction.
5. Check the home page and the service log. Confirm that Bitcoin Core, LND, Nostr,
   Twitter, and Telegram connect.
6. Point the reverse proxy to the Rust service. Restore traffic.
7. Create and pay one small Lightning request and one small on-chain request.
   Confirm the transaction ID, social posts, and Telegram notification.

## Bitcoin Core wallet notification

Configure `walletnotify` on the receiving wallet host. The exact command can
use a small wrapper that sets the two required variables:

```shell
export ORB_ADMIN_KEY_FILE=/run/secrets/wallet-notify-key
export ORB_WALLETNOTIFY_URL=http://127.0.0.1:9000/admin/walletnotify
exec /nix/store/...-op-return-bot/bin/op-return-bot-walletnotify "$1"
```

Bitcoin Core passes the transaction ID as `%s`. The final `walletnotify`
setting must call the wrapper with `%s`. The wrapper sends the key in the
`X-Wallet-Notify-Key` header. Do not proxy `/admin/walletnotify` to the
public internet. The handler rejects any peer that is not loopback, even
when the key is correct.

Bitcoin Core calls the command for every loaded wallet. Calls for
transactions that the receiving wallet does not know return `OK (0
payments)`. The service also lists recent labelled receives every 15
seconds. This scan recovers notifications that arrive while the service
is stopped.

The service spends the on-chain payment output itself when it creates the
OP_RETURN transaction for an on-chain payment. The receiving wallet must be
able to sign, so do not use a watch-only receiving wallet.

## Rollback

1. Stop traffic and stop the Rust service.
2. Keep the current database as evidence. Do not overwrite it.
3. If the Rust service did not create new requests, restart Scala with the
   pre-cutover backup.
4. If Rust accepted new requests, do not start Scala against either database.
   Keep Rust stopped, restore a test copy, and reconcile the new invoices and
   payments first. The new columns and tables are additive, but Scala does not
   know about requests created through ldk-server.

## ldk-server switch

The backend is selectable per deployment. Existing invoices record the backend
that created them. Before a switch from LND to ldk-server, let active LND
invoices expire or settle. Keep the LND data and credentials until all old
payments are reconciled. Then set `lightning.backend = "ldk-server"`, verify on
regtest, and deploy.
