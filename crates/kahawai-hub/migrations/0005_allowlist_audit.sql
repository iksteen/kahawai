-- Allowlist admission (SEC-5): the satellites table IS the mTLS allowlist;
-- no deny list to maintain. satellite_audit is an append-only record of
-- admissions and deletions (forensics + re-enrollment spam limiting).

CREATE TABLE satellite_audit (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id   TEXT NOT NULL DEFAULT '',
    fingerprint TEXT NOT NULL,
    action      TEXT NOT NULL, -- 'enrolled' | 'deleted'
    at          INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Preserve the old deny-list rows as audit history, then retire the table.
INSERT INTO satellite_audit (module_id, fingerprint, action, at)
SELECT '', fingerprint, 'deleted', revoked_at FROM revoked_certs;

DROP TABLE revoked_certs;
