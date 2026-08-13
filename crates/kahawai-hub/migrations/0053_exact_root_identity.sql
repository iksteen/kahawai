ALTER TABLE collections ADD COLUMN exact_roots_json TEXT NOT NULL DEFAULT '[]';

ALTER TABLE files ADD COLUMN root_token TEXT NOT NULL DEFAULT '';
ALTER TABLE files ADD COLUMN source_path TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX files_exact_source
    ON files (module_id, collection_id, root_token, source_path)
    WHERE root_token <> '';

ALTER TABLE item_sources ADD COLUMN root_token TEXT NOT NULL DEFAULT '';
ALTER TABLE item_sources ADD COLUMN source_path TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX item_sources_exact_source
    ON item_sources (module_id, collection_id, root_token, source_path)
    WHERE root_token <> '';

ALTER TABLE subtitle_tracks ADD COLUMN root_token TEXT;
ALTER TABLE subtitle_tracks ADD COLUMN source_path TEXT;
DROP INDEX subtitle_tracks_stream;
CREATE UNIQUE INDEX subtitle_tracks_stream ON subtitle_tracks
    (item_id, module_id, collection_id, root_token, source_path, origin, stream_index)
    WHERE origin IN ('embedded', 'sidecar');

ALTER TABLE image_set_failures ADD COLUMN root_token TEXT NOT NULL DEFAULT '';
ALTER TABLE image_set_failures ADD COLUMN source_path TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX image_set_failures_exact_source
    ON image_set_failures (module_id, collection_id, root_token, source_path, sub_index)
    WHERE root_token <> '';
