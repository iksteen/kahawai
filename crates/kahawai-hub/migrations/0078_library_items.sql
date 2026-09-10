DROP TRIGGER IF EXISTS catalog_dirty_collection_items_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_collection_items_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_collection_items_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_playable_sources_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_playable_sources_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_playable_sources_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_provider_metadata_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_provider_metadata_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_provider_metadata_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_item_match_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_item_match_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_item_match_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_manual_match_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_manual_match_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_manual_match_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_rejected_matches_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_rejected_matches_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_rejected_matches_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_assignment_pins_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_assignment_pins_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_assignment_pins_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_rejected_catalog_matches_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_rejected_catalog_matches_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_rejected_catalog_matches_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_state_imports_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_state_imports_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_state_imports_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_files_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_files_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_files_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_parts_INSERT;
DROP TRIGGER IF EXISTS catalog_dirty_parts_UPDATE;
DROP TRIGGER IF EXISTS catalog_dirty_parts_DELETE;
DROP TRIGGER IF EXISTS catalog_dirty_collection_type;

DROP VIEW IF EXISTS catalog_membership;
DROP VIEW IF EXISTS catalog_sources;
DROP VIEW IF EXISTS catalog_entries;
DROP VIEW collection_watch_state;

ALTER TABLE catalog_items RENAME TO library_items;
ALTER TABLE library_items RENAME COLUMN provisional TO unidentified;
ALTER TABLE library_items ADD COLUMN match_artist TEXT;
ALTER TABLE library_items ADD COLUMN edition TEXT;
ALTER TABLE library_items ADD COLUMN recording_id TEXT;

UPDATE library_items SET
    match_artist=(SELECT json_extract(key,'$[2]') FROM catalog_keys WHERE catalog_id=library_items.id AND json_extract(key,'$[0]')='album' LIMIT 1),
    edition=(SELECT json_extract(key,'$[4]') FROM catalog_keys WHERE catalog_id=library_items.id AND json_extract(key,'$[0]')='album' LIMIT 1),
    recording_id=(SELECT json_extract(key,'$[1]') FROM catalog_keys WHERE catalog_id=library_items.id AND json_extract(key,'$[0]')='recording:musicbrainz' LIMIT 1);
ALTER TABLE album_tracks RENAME TO album_copies_v77;
UPDATE album_copies_v77 SET disc_number=1 WHERE disc_number=0 AND EXISTS(
    SELECT 1 FROM collection_items ci WHERE ci.id=album_copies_v77.collection_item_id AND ci.season IS NULL);
DROP INDEX album_positions;
DROP INDEX album_songs;
CREATE TABLE album_tracks (
    id INTEGER PRIMARY KEY,
    album_id TEXT NOT NULL REFERENCES library_items(id),
    song_id TEXT NOT NULL REFERENCES library_items(id),
    disc_number INTEGER NOT NULL,
    track_number INTEGER NOT NULL,
    UNIQUE(album_id,disc_number,track_number,song_id)
);
CREATE INDEX album_songs ON album_tracks(song_id,album_id);
INSERT INTO album_tracks(album_id,song_id,disc_number,track_number)
SELECT DISTINCT album_id,song_id,disc_number,track_number FROM album_copies_v77;
INSERT INTO album_tracks(album_id,song_id,disc_number,track_number)
SELECT json_extract(key,'$[1]'),catalog_id,
    CASE WHEN json_extract(key,'$[2]')=0 AND EXISTS(
        SELECT 1 FROM album_copies_v77 a JOIN collection_items ci ON ci.id=a.collection_item_id
        WHERE a.album_id=json_extract(key,'$[1]') AND a.song_id=catalog_id AND a.track_number=json_extract(key,'$[3]') AND ci.season IS NULL)
        AND NOT EXISTS(SELECT 1 FROM album_copies_v77 a WHERE a.album_id=json_extract(key,'$[1]') AND a.song_id=catalog_id AND a.track_number=json_extract(key,'$[3]') AND a.disc_number=0)
        THEN 1 ELSE json_extract(key,'$[2]') END,json_extract(key,'$[3]')
FROM catalog_keys WHERE json_extract(key,'$[0]')='song'
ON CONFLICT DO NOTHING;
ALTER TABLE collection_items ADD COLUMN album_track_id INTEGER REFERENCES album_tracks(id);
UPDATE collection_items SET album_track_id=(
    SELECT t.id FROM album_copies_v77 a JOIN album_tracks t
      ON (t.album_id,t.song_id,t.disc_number,t.track_number)=(a.album_id,a.song_id,a.disc_number,a.track_number)
    WHERE a.collection_item_id=collection_items.id);
CREATE INDEX collection_album_track ON collection_items(album_track_id);
DROP TABLE album_copies_v77;

CREATE INDEX library_movie_series_match ON library_items(kind,year,sort_title) WHERE unidentified=0 AND merged_into IS NULL;
CREATE INDEX library_album_match ON library_items(kind,year,match_artist,sort_title,edition) WHERE unidentified=0 AND merged_into IS NULL;
CREATE INDEX library_recording_match ON library_items(recording_id) WHERE unidentified=0 AND merged_into IS NULL;
DROP TABLE catalog_keys;

ALTER TABLE collection_items ADD COLUMN assignment_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE collection_items ADD COLUMN assignment_manual INTEGER NOT NULL DEFAULT 0;
ALTER TABLE collection_items ADD COLUMN match_mode TEXT NOT NULL DEFAULT 'unmatched' CHECK(match_mode IN ('automatic','manual','unmatched'));
ALTER TABLE collection_items ADD COLUMN match_conflict TEXT;
ALTER TABLE collection_items ADD COLUMN metadata_eligible INTEGER NOT NULL DEFAULT 1;
UPDATE collection_items SET
    assignment_revision=COALESCE((SELECT revision FROM item_assignments WHERE collection_item_id=collection_items.id),0),
    assignment_manual=EXISTS(SELECT 1 FROM assignment_pins WHERE collection_item_id=collection_items.id),
    match_mode=COALESCE((SELECT mode FROM item_assignments WHERE collection_item_id=collection_items.id),'unmatched'),
    match_conflict=(SELECT conflict FROM item_assignments WHERE collection_item_id=collection_items.id),
    metadata_eligible=COALESCE((SELECT metadata_eligible FROM item_assignments WHERE collection_item_id=collection_items.id),1);

CREATE TABLE collection_item_library_items (
    collection_item_id TEXT NOT NULL REFERENCES collection_items(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    library_item_id TEXT NOT NULL REFERENCES library_items(id),
    PRIMARY KEY(collection_item_id,ordinal),
    UNIQUE(collection_item_id,library_item_id)
);
INSERT INTO collection_item_library_items SELECT collection_item_id,ordinal,catalog_id FROM assignment_members;
CREATE INDEX collection_library_item ON collection_item_library_items(library_item_id,collection_item_id);
DROP TABLE assignment_members;
DROP TABLE item_assignments;
DROP TABLE assignment_pins;

ALTER TABLE episode_details ADD COLUMN numbering TEXT NOT NULL DEFAULT 'aired';
UPDATE episode_details SET numbering=CASE WHEN season IS NULL THEN 'absolute' ELSE 'aired' END;
DROP TABLE episode_numberings;
CREATE INDEX library_episode_match ON episode_details(series_id,numbering,season,episode);

ALTER TABLE rejected_catalog_matches RENAME TO rejected_library_matches;
ALTER TABLE rejected_library_matches RENAME COLUMN catalog_id TO library_item_id;
ALTER TABLE catalog_overrides RENAME TO library_overrides;
ALTER TABLE library_overrides RENAME COLUMN catalog_id TO library_item_id;
ALTER TABLE catalog_pending RENAME TO library_pending;
ALTER TABLE state_imports RENAME COLUMN expected_catalog_id TO expected_library_item_id;
ALTER TABLE watch_state_archive RENAME COLUMN catalog_item_id TO library_item_id;
ALTER TABLE watch_source_archive RENAME COLUMN catalog_item_id TO library_item_id;

CREATE VIEW collection_watch_state AS
SELECT w.user_id,a.collection_item_id AS item_id,w.position_ms,w.duration_ms,
       w.played,w.play_count,w.updated_at,w.resume_source_fingerprint,w.item_id AS library_item_id
FROM collection_item_library_items a JOIN user_item_state w ON w.item_id=a.library_item_id
WHERE a.ordinal=1;

INSERT INTO user_prefs(user_id,scope,key,value)
SELECT p.user_id,a.library_item_id,p.key,p.value FROM user_prefs p
JOIN collection_item_library_items a ON a.collection_item_id=p.scope
WHERE p.key NOT IN('audio.track','subs.track') AND NOT(p.key='audio' AND p.value LIKE '#%')
ORDER BY p.scope ON CONFLICT DO NOTHING;
INSERT INTO user_prefs(user_id,scope,key,value)
SELECT p.user_id,'source:' || ps.id,CASE WHEN p.key='audio' THEN 'audio.track' ELSE p.key END,p.value
FROM user_prefs p JOIN playable_sources ps ON ps.item_id=p.scope
WHERE (p.key IN('audio.track','subs.track') OR (p.key='audio' AND p.value LIKE '#%'))
AND (SELECT COUNT(*) FROM playable_sources WHERE item_id=p.scope)=1
ON CONFLICT DO NOTHING;

INSERT INTO library_pending SELECT id FROM collection_items WHERE kind='episode' ON CONFLICT DO NOTHING;
