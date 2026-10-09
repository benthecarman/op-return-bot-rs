-- A fixed-amount BOLT12 offer for a request. Payments are matched by offer
-- ID, and the payment hash is stored once the offer is paid.
CREATE TABLE IF NOT EXISTS bolt12_offers (
    offer_id TEXT PRIMARY KEY NOT NULL,
    op_return_request_id INTEGER NOT NULL UNIQUE,
    offer TEXT NOT NULL,
    payment_hash TEXT,
    FOREIGN KEY (op_return_request_id) REFERENCES op_return_requests(id)
        ON DELETE CASCADE ON UPDATE NO ACTION
);
