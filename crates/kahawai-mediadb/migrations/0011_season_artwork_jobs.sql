DROP TRIGGER enrichment_item_insert;
DROP TRIGGER enrichment_item_revision;
CREATE TRIGGER enrichment_item_insert AFTER INSERT ON collection_items BEGIN
INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,p.provider,i.enrichment_revision FROM collection_items i
 JOIN (SELECT media_type,provider FROM provider_order UNION SELECT media_type,'local' FROM collections
 UNION SELECT 'anime','anidb-hash' UNION SELECT 'anime','anime-mappings' UNION SELECT media_type,'tmdb-artwork' FROM collections UNION SELECT media_type,'tvdb-artwork' FROM collections UNION SELECT 'anime','anilist-artwork' UNION SELECT 'anime','anidb-artwork' UNION SELECT 'series','fanart' UNION SELECT 'anime','fanart' UNION SELECT 'series','fanart-artwork' UNION SELECT 'anime','fanart-artwork' UNION SELECT media_type,'local-artwork' FROM collections UNION SELECT 'music','coverartarchive' UNION SELECT 'music','artist-collage' UNION SELECT 'music','fanart' UNION SELECT 'music','theaudiodb') p
 ON p.media_type=(SELECT media_type FROM collections WHERE id=i.collection_id)
 WHERE i.id=NEW.id AND EXISTS(SELECT 1 FROM library_collections WHERE collection_id=i.collection_id)
 ON CONFLICT(item_id,provider) DO UPDATE SET revision=excluded.revision,state='pending',due_at=0,token=NULL,lease_until=0,error=NULL;
END;
CREATE TRIGGER enrichment_item_revision AFTER UPDATE OF enrichment_revision ON collection_items BEGIN
INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,p.provider,i.enrichment_revision FROM collection_items i
 JOIN (SELECT media_type,provider FROM provider_order UNION SELECT media_type,'local' FROM collections
 UNION SELECT 'anime','anidb-hash' UNION SELECT 'anime','anime-mappings' UNION SELECT media_type,'tmdb-artwork' FROM collections UNION SELECT media_type,'tvdb-artwork' FROM collections UNION SELECT 'anime','anilist-artwork' UNION SELECT 'anime','anidb-artwork' UNION SELECT 'series','fanart' UNION SELECT 'anime','fanart' UNION SELECT 'series','fanart-artwork' UNION SELECT 'anime','fanart-artwork' UNION SELECT media_type,'local-artwork' FROM collections UNION SELECT 'music','coverartarchive' UNION SELECT 'music','artist-collage' UNION SELECT 'music','fanart' UNION SELECT 'music','theaudiodb') p
 ON p.media_type=(SELECT media_type FROM collections WHERE id=i.collection_id)
 WHERE i.id=NEW.id AND EXISTS(SELECT 1 FROM library_collections WHERE collection_id=i.collection_id)
 ON CONFLICT(item_id,provider) DO UPDATE SET revision=excluded.revision,state='pending',due_at=0,token=NULL,lease_until=0,error=NULL;
END;

DROP TRIGGER enrichment_description_art;
CREATE TRIGGER enrichment_description_art AFTER UPDATE OF description_json,season_artwork_json ON provider_records
WHEN NEW.description_json IS NOT OLD.description_json OR NEW.season_artwork_json IS NOT OLD.season_artwork_json BEGIN
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE provider IN ('tmdb-artwork','tvdb-artwork','anilist-artwork','anidb-artwork','fanart-artwork','local-artwork','coverartarchive','artist-collage')
 AND (item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM metadata_supplements WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM local_metadata WHERE record_id=NEW.id));
END;
CREATE TRIGGER enrichment_supplement_art AFTER INSERT ON metadata_supplements BEGIN
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE item_id=NEW.item_id AND provider=(SELECT CASE provider WHEN 'musicbrainz' THEN 'coverartarchive' ELSE provider||'-artwork' END FROM provider_records WHERE id=NEW.record_id);
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE item_id=NEW.item_id AND provider='fanart' AND state='done'
 AND EXISTS(SELECT 1 FROM provider_records WHERE id=NEW.record_id AND provider='tvdb' AND namespace='show');
END;
CREATE TRIGGER enrichment_tvdb_link AFTER INSERT ON provider_links WHEN NEW.provider='tvdb' AND NEW.namespace='show' BEGIN
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE provider='fanart' AND state='done'
 AND item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.record_id);
END;
