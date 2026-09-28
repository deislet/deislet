-- 0005_capability_issuances: every capability this control plane signs,
-- and revocations that name what they revoke.
--
-- 0003's capability_revocations held a jti and when it was revoked, and
-- nothing ever wrote to it. A revocation now has to say when the token it
-- names stops mattering - its expiry - so the list verifiers poll can leave
-- it out afterwards; the table is replaced rather than migrated.

CREATE TABLE capability_issuances (
    jti          TEXT NOT NULL PRIMARY KEY,  -- the capability's own id (UUIDv7)
    subject      TEXT NOT NULL,              -- `sub`: the SPIFFE name it was minted for
    node_id      TEXT,                       -- the node id inside `subject`, when it names a node
    audience     TEXT NOT NULL,              -- `aud`
    kid          TEXT NOT NULL,              -- the signing key's thumbprint
    auth_epoch   INTEGER NOT NULL,
    issued_at    INTEGER NOT NULL,           -- `iat`, Unix seconds
    expires_at   INTEGER NOT NULL            -- `exp`, Unix seconds
);

CREATE INDEX capability_issuances_node ON capability_issuances (node_id, expires_at)
    WHERE node_id IS NOT NULL;
CREATE INDEX capability_issuances_expiry ON capability_issuances (expires_at);

-- One row per (tenant, binding) a capability grants.
CREATE TABLE capability_issuance_resources (
    jti          TEXT NOT NULL REFERENCES capability_issuances (jti) ON DELETE CASCADE,
    tenant       TEXT NOT NULL,
    binding      TEXT NOT NULL,
    generation   TEXT
);

CREATE INDEX capability_issuance_resources_jti ON capability_issuance_resources (jti);
CREATE INDEX capability_issuance_resources_tenant ON capability_issuance_resources (tenant);

DROP TABLE capability_revocations;

CREATE TABLE capability_revocations (
    jti          TEXT NOT NULL PRIMARY KEY REFERENCES capability_issuances (jti) ON DELETE CASCADE,
    expires_at   INTEGER NOT NULL,           -- the revoked capability's `exp`
    revoked_at   INTEGER NOT NULL,           -- Unix seconds
    reason       TEXT NOT NULL               -- `jti`, `node:<id>` or `tenant:<tenant>`
);

CREATE INDEX capability_revocations_expiry ON capability_revocations (expires_at);

-- Nodes revoked as principals. Revoking the jtis a node holds lasts only
-- until it renews; a row here refuses the node everything on the machine
-- channel (registration, assignments, secrets, new capabilities) and puts it
-- on the signed revocation list, so every verifier refuses any capability
-- naming it, whenever it was issued. It stays until an operator lifts it.
CREATE TABLE revoked_nodes (
    node_id      TEXT NOT NULL PRIMARY KEY,
    revoked_at   INTEGER NOT NULL            -- Unix seconds
);
