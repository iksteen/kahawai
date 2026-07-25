-- HUB-23: record who initiated a subtitle download (auditability); the
-- subtitle itself stays attached to the item, available to everyone.
ALTER TABLE downloaded_subtitles ADD COLUMN downloaded_by TEXT;
