-- Messages were written as hex text into this BLOB column, the legacy
-- format, and older rows may hold raw bytes. Readers decoded any value
-- that was valid hex and kept the rest as it was. Apply that same rule
-- once, so every row holds the raw message bytes.
UPDATE op_return_requests
SET message_bytes = coalesce(
    unhex(CAST(message_bytes AS TEXT)),
    CAST(message_bytes AS BLOB)
);
