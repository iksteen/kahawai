ALTER TABLE users ADD COLUMN auth_version INTEGER NOT NULL DEFAULT 1;

UPDATE refresh_families
   SET revoked_at = unixepoch()
 WHERE revoked_at IS NULL;
