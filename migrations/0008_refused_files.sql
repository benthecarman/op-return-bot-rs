-- Files the decision service refused. Only hashes are kept, never the
-- file. A resubmission, or a near copy of a refused image, is refused
-- without asking the service again.
CREATE TABLE IF NOT EXISTS refused_files (
    sha256 TEXT PRIMARY KEY NOT NULL,
    -- 64-bit difference hash of an image, stored as a signed integer.
    dhash INTEGER,
    size INTEGER NOT NULL,
    kind TEXT NOT NULL,
    refused_at INTEGER NOT NULL
);
