-- 0003_capability_signing: Ed25519 signing key storage and auth-epoch
-- for RB-1 capability issuance (ADR-025).

-- One active signing key at a time, identified by its public key bytes.
-- The private key is stored here encrypted with the control plane's master
-- key, but the master key may be absent; in that case a fresh key is
-- generated on every start and never persisted (single-box mode).
CREATE TABLE capability_signing_keys (
    key_id         TEXT NOT NULL PRIMARY KEY,   -- hex-encoded public key
    public_key     BLOB NOT NULL,               -- 32 raw bytes
    private_key    BLOB NOT NULL,               -- 32 raw bytes (encrypted with master key, or plain in single-box)
    created_at     INTEGER NOT NULL,            -- Unix seconds
    active         INTEGER NOT NULL DEFAULT 1   -- boolean; only one row may have active=1
);

-- Singleton row: the current auth_epoch. Bumped to force every node to
-- fetch a fresh capability after a key rotation or fleet-wide revocation.
CREATE TABLE capability_auth_epoch (
    id             INTEGER PRIMARY KEY CHECK (id = 1), -- singleton
    epoch          INTEGER NOT NULL DEFAULT 1,
    created_at     INTEGER NOT NULL
);

-- Revoked capabilities. Rows older than the maximum token lifetime
-- (15 minutes) may be pruned periodically.
CREATE TABLE capability_revocations (
    jti            TEXT NOT NULL PRIMARY KEY,
    revoked_at     INTEGER NOT NULL     -- Unix seconds
);