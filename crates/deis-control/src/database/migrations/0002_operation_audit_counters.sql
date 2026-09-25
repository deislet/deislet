-- Migration 0002: operation audit counters.
--
-- Example of the post-baseline migration shape: a small, additive change.
-- This table holds the per-day audit counters the operation audit endpoint
-- aggregates; it is created empty and filled by the writer going forward.
CREATE TABLE operation_audit_counters (
            day          TEXT NOT NULL CHECK(length(day) = 10),
            outcome      TEXT NOT NULL,
            counter      INTEGER NOT NULL,
            PRIMARY KEY (day, outcome)
);
