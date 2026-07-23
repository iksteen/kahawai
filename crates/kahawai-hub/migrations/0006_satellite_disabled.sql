-- Admin drain toggle survives hub restarts: a disabled transcoder must
-- not silently rejoin placement because the hub bounced.
ALTER TABLE satellites ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0;
