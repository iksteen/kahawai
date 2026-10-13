ALTER TABLE enrichment_jobs ADD COLUMN request_generation INTEGER NOT NULL DEFAULT 0;
CREATE TABLE enrichment_lanes(media_type TEXT NOT NULL,provider TEXT NOT NULL,PRIMARY KEY(media_type,provider));
INSERT INTO enrichment_lanes VALUES
('movies','local'),
('movies','local-artwork'),
('movies','tmdb-artwork'),
('movies','tvdb-artwork'),
('series','local'),
('series','local-artwork'),
('series','tmdb-artwork'),
('series','tvdb-artwork'),
('anime','local'),
('anime','local-artwork'),
('anime','tmdb-artwork'),
('anime','tvdb-artwork'),
('music','local'),
('music','local-artwork'),
('music','tmdb-artwork'),
('music','tvdb-artwork'),
('anime','anidb-hash'),
('anime','anime-mappings'),
('anime','anilist-artwork'),
('anime','anidb-artwork'),
('anime','fanart'),
('anime','fanart-artwork'),
('series','fanart'),
('series','fanart-artwork'),
('music','coverartarchive'),
('music','artist-collage'),
('music','fanart'),
('music','theaudiodb');
DROP TRIGGER enrichment_item_insert;
CREATE TRIGGER enrichment_item_insert AFTER INSERT ON collection_items BEGIN
 INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,p.provider,i.enrichment_revision FROM collection_items i
 JOIN (SELECT media_type,provider FROM provider_order UNION SELECT media_type,provider FROM enrichment_lanes) p
 ON p.media_type=(SELECT media_type FROM collections WHERE id=i.collection_id)
 WHERE i.id=NEW.id AND EXISTS(SELECT 1 FROM library_collections WHERE collection_id=i.collection_id)
 ON CONFLICT(item_id,provider) DO UPDATE SET revision=excluded.revision,request_generation=enrichment_jobs.request_generation+1,state='pending',due_at=0,token=NULL,lease_until=0,error=NULL;
END;
DROP TRIGGER enrichment_item_revision;
CREATE TRIGGER enrichment_item_revision AFTER UPDATE OF enrichment_revision ON collection_items BEGIN
 INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,p.provider,i.enrichment_revision FROM collection_items i
 JOIN (SELECT media_type,provider FROM provider_order UNION SELECT media_type,provider FROM enrichment_lanes) p
 ON p.media_type=(SELECT media_type FROM collections WHERE id=i.collection_id)
 WHERE i.id=NEW.id AND EXISTS(SELECT 1 FROM library_collections WHERE collection_id=i.collection_id)
 ON CONFLICT(item_id,provider) DO UPDATE SET revision=excluded.revision,request_generation=enrichment_jobs.request_generation+1,state='pending',due_at=0,token=NULL,lease_until=0,error=NULL;
END;
DROP TRIGGER enrichment_description_art;
DROP TRIGGER enrichment_supplement_art;
DROP TRIGGER enrichment_tvdb_link;
DROP TRIGGER enrichment_local_art;
DROP TRIGGER enrichment_collage_membership;
CREATE TRIGGER enrichment_description_art AFTER UPDATE OF description_json,season_artwork_json ON provider_records
WHEN NEW.description_json IS NOT OLD.description_json OR NEW.season_artwork_json IS NOT OLD.season_artwork_json BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE provider IN ('tmdb-artwork','tvdb-artwork','anilist-artwork','anidb-artwork','fanart-artwork','local-artwork','coverartarchive','artist-collage')
 AND (item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM metadata_supplements WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM local_metadata WHERE record_id=NEW.id));
END;
CREATE TRIGGER enrichment_supplement_art AFTER INSERT ON metadata_supplements BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE item_id=NEW.item_id AND provider=(SELECT CASE provider WHEN 'musicbrainz' THEN 'coverartarchive' ELSE provider||'-artwork' END FROM provider_records WHERE id=NEW.record_id);
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE item_id=NEW.item_id AND provider='fanart'
 AND EXISTS(SELECT 1 FROM provider_records WHERE id=NEW.record_id AND provider='tvdb' AND namespace='show');
END;
CREATE TRIGGER enrichment_tvdb_link AFTER INSERT ON provider_links WHEN NEW.provider='tvdb' AND NEW.namespace='show' BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE provider='fanart'
 AND item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.record_id);
END;
CREATE TRIGGER enrichment_local_art AFTER INSERT ON local_metadata BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE item_id=NEW.item_id AND provider IN ('local-artwork','artist-collage');
END;
CREATE TRIGGER enrichment_collage_membership AFTER DELETE ON library_collections BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE provider='artist-collage' AND item_id IN
 (SELECT i.id FROM collection_items i JOIN library_collections lc ON lc.collection_id=i.collection_id WHERE lc.library_id=OLD.library_id);
END;
INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,l.provider,i.enrichment_revision FROM collection_items i JOIN collections c ON c.id=i.collection_id
 JOIN metadata_assignments a ON a.item_id=i.id
 JOIN (SELECT media_type,provider FROM enrichment_lanes UNION SELECT media_type,provider FROM provider_order) l ON l.media_type=c.media_type
 WHERE c.media_type IN ('series','anime')
 AND EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id)
 AND EXISTS(SELECT 1 FROM media_entries e JOIN media_parts p ON p.entry_id=e.id WHERE e.item_id=i.id)
 AND (l.provider IN ('local','local-artwork','tmdb-artwork','tvdb-artwork','anidb-artwork','anilist-artwork','fanart-artwork','fanart','anime-mappings')
 OR (l.provider IN ('tmdb','tvdb','anidb','anilist') AND (
 EXISTS(SELECT 1 FROM provider_records p WHERE p.id=a.record_id AND p.provider=l.provider AND p.namespace IN ('show','anime'))
 OR EXISTS(SELECT 1 FROM provider_links p WHERE p.record_id=a.record_id AND p.provider=l.provider AND p.namespace IN ('show','anime'))
 OR EXISTS(SELECT 1 FROM metadata_supplements p JOIN provider_records r ON r.id=p.record_id WHERE p.item_id=i.id AND p.primary_record_id=a.record_id AND r.provider=l.provider AND r.namespace IN ('show','anime')))))
 ON CONFLICT(item_id,provider) DO UPDATE SET
 request_generation=enrichment_jobs.request_generation+1,
 state=CASE WHEN enrichment_jobs.state='done' THEN 'pending' ELSE enrichment_jobs.state END,
 due_at=CASE WHEN enrichment_jobs.state='done' THEN 0 ELSE enrichment_jobs.due_at END;
