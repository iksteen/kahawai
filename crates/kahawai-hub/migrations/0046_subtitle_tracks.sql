-- One row per subtitle track, every origin. Semantics: the module doc
-- of kahawai-hub/src/tracks.rs (house rule: schema meaning lives next
-- to the code that enforces it).
CREATE TABLE subtitle_tracks (
    id            INTEGER PRIMARY KEY,
    item_id       TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    origin        TEXT NOT NULL,
    module_id     TEXT,
    collection_id TEXT,
    path_rel      TEXT,
    stream_index  INTEGER,
    format        TEXT NOT NULL,
    language      TEXT,
    label         TEXT,
    provider      TEXT,
    machine       INTEGER NOT NULL DEFAULT 0,
    created_by    TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    derived_from  INTEGER REFERENCES subtitle_tracks(id) ON DELETE SET NULL
);
CREATE INDEX subtitle_tracks_item ON subtitle_tracks (item_id);
CREATE UNIQUE INDEX subtitle_tracks_stream ON subtitle_tracks
    (item_id, module_id, collection_id, path_rel, origin, stream_index)
    WHERE origin IN ('embedded', 'sidecar');

-- Downloaded/OCR rows move in first, PRESERVING their ids: the cached
-- cue bodies on disk are named downloaded-{id}.json and stay put.
INSERT INTO subtitle_tracks
    (id, item_id, origin, format, language, label, provider, machine,
     created_by, created_at)
SELECT id,
       item_id,
       CASE WHEN provider = 'ocr' THEN 'ocr' ELSE 'downloaded' END,
       format,
       language,
       release_name,
       provider,
       CASE WHEN provider = 'ocr' THEN 1 ELSE 0 END,
       downloaded_by,
       created_at
FROM downloaded_subtitles;

-- Embedded tracks, from every source's probed stream list.
INSERT INTO subtitle_tracks
    (item_id, origin, module_id, collection_id, path_rel, stream_index,
     format, language)
SELECT s.item_id, 'embedded', s.module_id, s.collection_id, s.path_rel,
       j.key,
       COALESCE(json_extract(j.value, '$.format'), 'text'),
       json_extract(j.value, '$.language')
FROM item_sources s
JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
             = (s.module_id, s.collection_id, s.path_rel),
     json_each(COALESCE(json_extract(f.streams_json, '$.subtitles'), '[]')) j;

-- Sidecar tracks. Source binding is the MEDIA file (list filtering and
-- the load path both key on it); stream_index is the row's position in
-- external_subtitles, which is how serving finds the entry — a VobSub
-- entry's in-idx track id lives in that entry, not here.
INSERT INTO subtitle_tracks
    (item_id, origin, module_id, collection_id, path_rel, stream_index,
     format, language)
SELECT s.item_id, 'sidecar', s.module_id, s.collection_id, s.path_rel,
       j.key,
       COALESCE(json_extract(j.value, '$.format'), 'srt'),
       json_extract(j.value, '$.language')
FROM item_sources s
JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
             = (s.module_id, s.collection_id, s.path_rel),
     json_each(COALESCE(json_extract(f.streams_json, '$.external_subtitles'), '[]')) j;

DROP TABLE downloaded_subtitles;
