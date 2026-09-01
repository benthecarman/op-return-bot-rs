# Compatibility contract

The Rust service keeps these production interfaces.

## Web and API routes

- Pages: `/`, `/nip5`, `/invoice`, `/success`, and `/connect`
- Form actions: `/createRequest` and `/createNip5Request`
- Nostr and LNURL: `/.well-known/nostr.json`,
  `/.well-known/lnurlp/{user}`, and `/lnurlp/{metadata}`
- Utilities: `/qr` and `/admin/walletnotify`
- REST: `/api/create`, `/api/unified`, `/api/status/{rHash}`,
  `/api/view/{txId}`, and `/api/mempool-limit`
- MCP: `/mcp` and `/.well-known/mcp.json`
- Agent metadata: `/sitemap.xml`, `/auth.md`,
  `/.well-known/api-catalog`, `/.well-known/oauth-protected-resource`, and
  `/.well-known/agent-skills/...`
- Static assets: `/assets/...`

The REST create routes accept both form data and JSON. Response field names and
the existing plain-text status responses are unchanged.

## Data and payments

- Existing SQLite rows remain in place. Legacy hex-encoded message and fee-rate
  values remain readable and new rows use the same encoding.
- Lightning invoices for requests commit to the SHA-256 of the message in
  their description hash, as before. The invoice page shows the same hash.
- New requests are accepted while the mempool chain limit is active. The
  broadcast waits for the next block, as before. `/api/mempool-limit` reports
  the state.
- New migrations add only backend, amount, zap, and service-state data.
- The service uses separate Bitcoin Core sending and receiving wallets. It
  loads both wallets at startup when Bitcoin Core has not loaded them.
- Receiving and change addresses are `bech32m`, as before.
- Unified payments accept zero-confirmation on-chain payments. The OP_RETURN
  transaction then spends the payment output from the receiving wallet, so a
  replaced payment also removes the OP_RETURN transaction. Requests paid by
  Lightning spend the sending wallet.
- After an on-chain payment the service cancels the Lightning invoice of the
  same request on LND. ldk-server cannot cancel invoices.
- The `walletnotify` endpoint remains the main on-chain trigger. It replies
  at once and processes the transaction in the background, as before.
- The service subscribes to invoice updates from both LND and ldk-server and
  reconnects when a stream ends. It does not poll open Lightning invoices.
  The ldk-server event stream is live-only, so `/processunhandled` remains the
  manual recovery path for a payment received while the subscriber was down.
  A 15-second reconciliation pass checks on-chain payments with one wallet
  call, retries paid requests, closes expired requests, and publishes zap
  receipts. The reconciliation interval is configurable.
- The chain fee and virtual size of a request are written when its
  transaction is published, as before. The signed transaction itself is
  stored before broadcast.
- Unpaid Lightning-only requests close one hour after the invoice expires.
  Unpaid requests with an on-chain address close after the on-chain expiry,
  which is seven days by default, as before.
  A request is never closed while its invoice is settled on the backend,
  while the backend cannot answer, or after a payment arrived.
- LND is the default Lightning backend. ldk-server is selectable.

## Transaction and price rules

- The transaction is version 2 with one input, a maximum sequence, change as
  output zero, OP_RETURN as output one, and lock time 106. The input is the
  most confirmed output of the sending wallet that can pay the fee, with
  unconfirmed outputs as the fallback, as before.
- The standard and non-standard fee formulas, application fee, privacy fee,
  non-standard fee, and 99,000-byte limit remain unchanged. Fee oracles
  must return 1 to 1,000 sat/vB. A value outside that range is rejected.
- The chain fee is the fee rate multiplied by the virtual size of the signed
  transaction, so large messages pay the intended rate.
- Standard transactions also go to Esplora. All transactions go to MARA
  Slipstream. Slipstream is the fallback only when Bitcoin Core rejects a
  non-standard transaction with a standardness reason: `scriptpubkey`,
  `datacarrier`, `tx-size`, or `multi-op-return`. The service then locks the
  spent output in the wallet so that the next request cannot spend it again.
  A mempool chain limit or any other rejection keeps the request paid and
  unpublished, and the reconciler retries it through Bitcoin Core.
- LNURL-pay invoices commit to the metadata hash in their description hash,
  as LUD-06 requires. The advertised maximum is 2,000,000 satoshis.

## Intentional fixes

- Repeated messages can create separate invoices.
- UTF-8 message limits count bytes, not characters.
- NIP-05 request data is stored in the same transaction as its payment data.
  Closing an unpaid request deletes its NIP-05 row so the name can be bought
  again. Names that were already stuck on closed unpaid requests are removed
  by a one-time migration.
- A signed Bitcoin transaction is stored before broadcast and can be retried.
- Concurrent settlements cannot spend the same wallet output.
- Paid requests recover after a crash between payment detection and broadcast.
- Literal banned words do not act as regular expressions.
- Nostr zap invoices use the required description hash, and the zap receipt
  carries the zap request exactly as received, so the hash verifies.
- Zap receipts also go to the public `wss://` relays named in the zap
  request. Loopback, private, and link-local hosts are ignored.
- The home page lists only public transactions. The Scala service also showed
  private transactions until its next restart.
- The wallet notification endpoint rejects every call when its key file is
  empty. It also rejects peers that are not loopback. The shared key is
  sent in `X-Wallet-Notify-Key` and compared in constant time.
- One failing request no longer stops reconciliation for other requests. A
  stored transaction whose input disappeared is rebuilt.
- Regtest invoices are rejected on mainnet.
- The Telegram bot skips a message that it cannot handle instead of retrying
  it forever, and it truncates long messages.
- Telegram `/report` accepts the legacy `h`, `hr`, `d`, `w`, `m`, and `y`
  time ranges. `/processunhandled` accepts its legacy count and mempool-limit
  arguments. Each completed purchase sends the full accounting notification
  to the configured admin, plus the separate buyer confirmation for Telegram
  purchases.
- Cross-origin requests are allowed on the API, LNURL, NIP-05, and MCP
  routes only.
- Internal, database, and upstream errors return a generic `internal
  error` body. The detailed message stays in the service log.
- Create routes share a process-wide rate limit. The default is 10
  invoices per IP per minute and 120 invoices per minute for the process.
  Telegram create commands count against the same budget. Set a limit to
  0 to disable it.
- The MCP server accepts the onion host.

## Not carried over

These features of the Scala service are not part of the Rust service.

- Requests through Nostr direct messages and the kind 5901 DVM job flow.
- Publishing the Nostr profile, contact list, and DVM handler advertisement.
- The Telegram commands `/checktxids`, `/utxos`, `/publicreport`, and
  `/fakezap`.
- Telegram alerts for zaps and for the mempool limit.
- The extra Nostr key for zaps and the write-only relay list.
