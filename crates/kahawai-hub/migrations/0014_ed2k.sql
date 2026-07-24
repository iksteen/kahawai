-- MH-9/HUB-30: full-file ED2K hashes (eMule/AniDB variant), computed by
-- the mediahost's idle hasher; NULL until reported. Cleared when a file's
-- content changes.
ALTER TABLE files ADD COLUMN ed2k TEXT;
