CREATE TABLE downloaded_subtitles (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 media_entry_id TEXT NOT NULL,
 source_version TEXT NOT NULL,
 provider TEXT NOT NULL,
 provider_file_id TEXT NOT NULL,
 format TEXT NOT NULL,
 language TEXT,
 label TEXT,
 created_by TEXT NOT NULL,
 payload TEXT NOT NULL,
 UNIQUE(media_entry_id,source_version,provider,provider_file_id)
);
