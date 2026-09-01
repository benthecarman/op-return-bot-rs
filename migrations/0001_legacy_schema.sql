CREATE TABLE IF NOT EXISTS op_return_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_bytes BLOB NOT NULL,
    no_twitter INTEGER NOT NULL,
    fee_rate TEXT NOT NULL,
    node_id TEXT,
    telegram_id INTEGER,
    nostr_key TEXT,
    dvm_event TEXT,
    time INTEGER NOT NULL,
    "transaction" TEXT,
    txid TEXT,
    profit INTEGER,
    chain_fee INTEGER,
    vsize INTEGER,
    closed INTEGER NOT NULL DEFAULT 0,
    btc_price INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS invoices (
    r_hash TEXT PRIMARY KEY NOT NULL,
    op_return_request_id INTEGER NOT NULL,
    invoice TEXT NOT NULL,
    paid INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (op_return_request_id) REFERENCES op_return_requests(id)
        ON DELETE CASCADE ON UPDATE NO ACTION
);

CREATE TABLE IF NOT EXISTS nip5 (
    op_return_request_id INTEGER PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    public_key TEXT NOT NULL,
    FOREIGN KEY (op_return_request_id) REFERENCES op_return_requests(id)
        ON DELETE CASCADE ON UPDATE NO ACTION
);

CREATE TABLE IF NOT EXISTS zaps (
    r_hash TEXT PRIMARY KEY NOT NULL,
    invoice TEXT UNIQUE NOT NULL,
    my_key TEXT NOT NULL,
    amount INTEGER NOT NULL,
    request TEXT NOT NULL,
    note_id TEXT UNIQUE,
    time INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS on_chain_payments (
    address TEXT PRIMARY KEY NOT NULL,
    op_return_request_id INTEGER NOT NULL,
    expected_amount INTEGER NOT NULL,
    amount_paid INTEGER,
    txid TEXT,
    FOREIGN KEY (op_return_request_id) REFERENCES op_return_requests(id)
        ON DELETE CASCADE ON UPDATE NO ACTION
);

CREATE UNIQUE INDEX IF NOT EXISTS op_return_requests_txid_index
    ON op_return_requests(txid);
CREATE INDEX IF NOT EXISTS op_return_requests_closed_index
    ON op_return_requests(closed);
CREATE INDEX IF NOT EXISTS op_return_requests_time_index
    ON op_return_requests(time);
CREATE INDEX IF NOT EXISTS op_return_requests_analytics_index
    ON op_return_requests(time, txid, profit, chain_fee, vsize, btc_price);
CREATE UNIQUE INDEX IF NOT EXISTS payments_invoice_idx ON invoices(invoice);
CREATE UNIQUE INDEX IF NOT EXISTS payments_op_return_request_id_idx
    ON invoices(op_return_request_id);
CREATE INDEX IF NOT EXISTS payments_paid_idx ON invoices(paid);
CREATE UNIQUE INDEX IF NOT EXISTS on_chain_payments_op_return_request_id_idx
    ON on_chain_payments(op_return_request_id);
CREATE INDEX IF NOT EXISTS on_chain_payments_txid_idx
    ON on_chain_payments(txid);
CREATE UNIQUE INDEX IF NOT EXISTS nip5_name_unique_idx ON nip5(lower(name));
