-- Unpaid NIP-05 rows reserved the name forever after the request closed.
-- Keep only names that are still open or already written on-chain.
DELETE FROM nip5
WHERE op_return_request_id IN (
    SELECT id FROM op_return_requests WHERE closed = 1 AND txid IS NULL
);
