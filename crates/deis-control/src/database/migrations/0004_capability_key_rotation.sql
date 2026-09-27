-- 0004_capability_key_rotation: signing keys named by kid, encrypted at
-- rest, with a rotation lifecycle.
--
-- 0003's table held one key named by its hex public key, optionally in the
-- clear. No release ever persisted a key into it (the issuer was in-memory
-- only), so it is replaced rather than migrated.

DROP TABLE capability_signing_keys;

CREATE TABLE capability_signing_keys (
    kid              TEXT NOT NULL PRIMARY KEY,  -- RFC 7638 thumbprint of the public key
    public_key       BLOB NOT NULL,              -- 32 raw bytes
    private_key      BLOB NOT NULL,              -- AES-256-GCM (nonce || ciphertext) under the secrets master key, kid as AAD
    state            TEXT NOT NULL CHECK (state IN ('pending', 'active', 'previous')),
    created_at       INTEGER NOT NULL,           -- Unix seconds
    state_changed_at INTEGER NOT NULL            -- Unix seconds of the last state change
);

-- At most one key signs.
CREATE UNIQUE INDEX capability_signing_keys_one_active
    ON capability_signing_keys (state) WHERE state = 'active';
