ALTER TABLE invoices
    ADD COLUMN lightning_backend TEXT NOT NULL DEFAULT 'lnd'
        CHECK (lightning_backend IN ('lnd', 'ldk-server'));

ALTER TABLE invoices ADD COLUMN claim_preimage BLOB;
