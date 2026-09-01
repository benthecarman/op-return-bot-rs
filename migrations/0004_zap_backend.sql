ALTER TABLE zaps ADD COLUMN lightning_backend TEXT NOT NULL DEFAULT 'lnd'
    CHECK (lightning_backend IN ('lnd', 'ldk-server'));
