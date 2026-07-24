-- SEC-7 certificate renewal: a renewed cert's fingerprint is admitted
-- alongside the current one until the satellite reconnects with it, or a
-- 24 h grace lapses — a satellite can never be locked out mid-renewal.

ALTER TABLE satellites ADD COLUMN pending_fingerprint TEXT;
ALTER TABLE satellites ADD COLUMN pending_issued_at INTEGER;
