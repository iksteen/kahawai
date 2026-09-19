ALTER TABLE collection_items ADD COLUMN enrichment_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE metadata_assignments ADD COLUMN manual INTEGER NOT NULL DEFAULT 0 CHECK(manual IN (0,1));
ALTER TABLE metadata_assignments ADD COLUMN strength INTEGER NOT NULL DEFAULT 10;
CREATE TABLE enrichment_jobs (
 item_id TEXT NOT NULL REFERENCES collection_items(id) ON DELETE CASCADE,
 provider TEXT NOT NULL, revision INTEGER NOT NULL,
 force INTEGER NOT NULL DEFAULT 0 CHECK(force IN (0,1)),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','running','retry','blocked','done')),
 due_at INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0,
 token TEXT, lease_until INTEGER NOT NULL DEFAULT 0, error TEXT,
 PRIMARY KEY(item_id,provider)
);
CREATE INDEX enrichment_due ON enrichment_jobs(provider,state,due_at,lease_until);
CREATE TABLE enrichment_candidates (
 item_id TEXT NOT NULL REFERENCES collection_items(id) ON DELETE CASCADE,
 record_id TEXT NOT NULL REFERENCES provider_records(id), revision INTEGER NOT NULL,
 strength INTEGER NOT NULL, PRIMARY KEY(item_id,record_id)
);
CREATE TABLE metadata_rejections (
 item_id TEXT NOT NULL REFERENCES collection_items(id) ON DELETE CASCADE,
 record_id TEXT NOT NULL REFERENCES provider_records(id), PRIMARY KEY(item_id,record_id)
);
CREATE TABLE provider_links (
 record_id TEXT NOT NULL REFERENCES provider_records(id), provider TEXT NOT NULL,
 namespace TEXT NOT NULL, external_id TEXT NOT NULL,
 PRIMARY KEY(record_id,provider,namespace,external_id)
);
CREATE TABLE local_metadata (
 item_id TEXT PRIMARY KEY REFERENCES collection_items(id) ON DELETE CASCADE,
 record_id TEXT NOT NULL REFERENCES provider_records(id)
);
CREATE TABLE enrichment_cache (
 provider TEXT NOT NULL, question TEXT NOT NULL, answer TEXT NOT NULL,
 updated_at INTEGER NOT NULL, PRIMARY KEY(provider,question)
);
CREATE TABLE enrichment_providers (
 provider TEXT PRIMARY KEY, due_at INTEGER NOT NULL DEFAULT 0,
 blocked INTEGER NOT NULL DEFAULT 0, error TEXT
);
CREATE TABLE artist_artwork (
 artist_id TEXT NOT NULL, provider TEXT NOT NULL, image_url TEXT,
 updated_at INTEGER NOT NULL, PRIMARY KEY(artist_id,provider)
);
CREATE TRIGGER enrichment_item_insert AFTER INSERT ON collection_items BEGIN
INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,p.provider,i.enrichment_revision FROM collection_items i
 JOIN (SELECT media_type,provider FROM provider_order UNION SELECT media_type,'local' FROM collections
 UNION SELECT 'anime','anidb-hash' UNION SELECT 'anime','anime-mappings' UNION SELECT media_type,'tmdb-artwork' FROM collections UNION SELECT media_type,'tvdb-artwork' FROM collections UNION SELECT 'anime','anilist-artwork' UNION SELECT media_type,'local-artwork' FROM collections UNION SELECT 'music','coverartarchive' UNION SELECT 'music','artist-collage' UNION SELECT 'music','fanart' UNION SELECT 'music','theaudiodb') p
 ON p.media_type=(SELECT media_type FROM collections WHERE id=i.collection_id)
 WHERE i.id=NEW.id AND EXISTS(SELECT 1 FROM library_collections WHERE collection_id=i.collection_id)
 ON CONFLICT(item_id,provider) DO UPDATE SET revision=excluded.revision,state='pending',due_at=0,token=NULL,lease_until=0,error=NULL;
END;
CREATE TRIGGER enrichment_item_revision AFTER UPDATE OF enrichment_revision ON collection_items BEGIN
INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,p.provider,i.enrichment_revision FROM collection_items i
 JOIN (SELECT media_type,provider FROM provider_order UNION SELECT media_type,'local' FROM collections
 UNION SELECT 'anime','anidb-hash' UNION SELECT 'anime','anime-mappings' UNION SELECT media_type,'tmdb-artwork' FROM collections UNION SELECT media_type,'tvdb-artwork' FROM collections UNION SELECT 'anime','anilist-artwork' UNION SELECT media_type,'local-artwork' FROM collections UNION SELECT 'music','coverartarchive' UNION SELECT 'music','artist-collage' UNION SELECT 'music','fanart' UNION SELECT 'music','theaudiodb') p
 ON p.media_type=(SELECT media_type FROM collections WHERE id=i.collection_id)
 WHERE i.id=NEW.id AND EXISTS(SELECT 1 FROM library_collections WHERE collection_id=i.collection_id)
 ON CONFLICT(item_id,provider) DO UPDATE SET revision=excluded.revision,state='pending',due_at=0,token=NULL,lease_until=0,error=NULL;
END;
CREATE TRIGGER enrichment_detected AFTER UPDATE OF title,year,artist,description_json ON collection_items
WHEN NEW.title IS NOT OLD.title OR NEW.year IS NOT OLD.year OR NEW.artist IS NOT OLD.artist OR NEW.description_json IS NOT OLD.description_json
BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=NEW.id; END;
CREATE TRIGGER enrichment_membership AFTER INSERT ON library_collections BEGIN
 UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE collection_id=NEW.collection_id
 AND (SELECT count(*) FROM library_collections WHERE collection_id=NEW.collection_id)=1;
END;
CREATE TRIGGER enrichment_media_parts_insert AFTER INSERT ON media_parts BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=(SELECT item_id FROM media_entries WHERE id=NEW.entry_id); END;
CREATE TRIGGER enrichment_media_parts_delete AFTER DELETE ON media_parts BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=(SELECT item_id FROM media_entries WHERE id=OLD.entry_id); END;
CREATE TRIGGER enrichment_media_parts_update AFTER UPDATE ON media_parts BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=(SELECT item_id FROM media_entries WHERE id=NEW.entry_id); END;
CREATE TRIGGER enrichment_source_facts_insert AFTER INSERT ON source_facts BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id IN (SELECT e.item_id FROM media_entries e JOIN media_parts p ON p.entry_id=e.id WHERE p.file_id=NEW.file_id); END;
CREATE TRIGGER enrichment_source_facts_delete AFTER DELETE ON source_facts BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id IN (SELECT e.item_id FROM media_entries e JOIN media_parts p ON p.entry_id=e.id WHERE p.file_id=OLD.file_id); END;
CREATE TRIGGER enrichment_source_facts_update AFTER UPDATE ON source_facts BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id IN (SELECT e.item_id FROM media_entries e JOIN media_parts p ON p.entry_id=e.id WHERE p.file_id=NEW.file_id); END;
CREATE TRIGGER enrichment_file AFTER UPDATE OF media_json,size,mtime ON files
WHEN NEW.media_json IS NOT OLD.media_json OR NEW.size IS NOT OLD.size OR NEW.mtime IS NOT OLD.mtime
BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id IN
(SELECT e.item_id FROM media_entries e JOIN media_parts p ON p.entry_id=e.id WHERE p.file_id=NEW.id); END;
CREATE TRIGGER enrichment_assignment_insert AFTER INSERT ON metadata_assignments BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=NEW.item_id; END;
CREATE TRIGGER enrichment_assignment_update AFTER UPDATE ON metadata_assignments BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=NEW.item_id; END;
CREATE TRIGGER enrichment_assignment_delete AFTER DELETE ON metadata_assignments BEGIN UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE id=OLD.item_id; END;
UPDATE collection_items SET enrichment_revision=enrichment_revision+1 WHERE EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=collection_items.collection_id);
CREATE TABLE artists (id TEXT PRIMARY KEY,name TEXT NOT NULL);
CREATE TABLE collection_item_artists (
 item_id TEXT PRIMARY KEY REFERENCES collection_items(id) ON DELETE CASCADE,
 artist_id TEXT NOT NULL REFERENCES artists(id),revision INTEGER NOT NULL
);
CREATE INDEX artist_copies ON collection_item_artists(artist_id,item_id);
CREATE TRIGGER enrichment_local_art AFTER INSERT ON local_metadata BEGIN
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE item_id=NEW.item_id AND provider IN ('local-artwork','artist-collage');
END;
CREATE TRIGGER enrichment_description_art AFTER UPDATE OF description_json ON provider_records
WHEN NEW.description_json<>OLD.description_json BEGIN
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE provider IN ('tmdb-artwork','tvdb-artwork','anilist-artwork','local-artwork','coverartarchive','artist-collage')
 AND (item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM metadata_supplements WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM local_metadata WHERE record_id=NEW.id));
END;
CREATE TRIGGER enrichment_collage_membership AFTER DELETE ON library_collections BEGIN
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE provider='artist-collage' AND item_id IN
 (SELECT i.id FROM collection_items i JOIN library_collections lc ON lc.collection_id=i.collection_id WHERE lc.library_id=OLD.library_id);
END;
