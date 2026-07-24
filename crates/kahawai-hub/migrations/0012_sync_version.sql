-- Reconnect handshake (MH-5 extension): the mediahost's persisted scan
-- generation. Equal versions on reconnect = the hub already reflects
-- the host's last completed scan; the startup rescan is skipped.
ALTER TABLE collections ADD COLUMN sync_version INTEGER NOT NULL DEFAULT 0;
