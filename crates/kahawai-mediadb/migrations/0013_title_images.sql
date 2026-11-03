INSERT INTO enrichment_lanes VALUES ('movies','fanart'),('movies','fanart-artwork');

DROP TRIGGER enrichment_description_art;
CREATE TRIGGER enrichment_description_art AFTER UPDATE OF description_json,children_json,season_artwork_json ON provider_records
WHEN NEW.description_json IS NOT OLD.description_json OR NEW.children_json IS NOT OLD.children_json OR NEW.season_artwork_json IS NOT OLD.season_artwork_json BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE provider IN ('tmdb-artwork','tvdb-artwork','anilist-artwork','anidb-artwork','fanart-artwork','local-artwork','coverartarchive','artist-collage')
 AND (item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM metadata_supplements WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM local_metadata WHERE record_id=NEW.id));
END;

DROP TRIGGER enrichment_tvdb_link;
CREATE TRIGGER enrichment_tvdb_link AFTER INSERT ON provider_links
WHEN (NEW.provider='tvdb' AND NEW.namespace='show') OR (NEW.provider='tmdb' AND NEW.namespace='movie') BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE provider='fanart'
 AND item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.record_id);
END;

DROP TRIGGER enrichment_supplement_art;
CREATE TRIGGER enrichment_supplement_art AFTER INSERT ON metadata_supplements BEGIN
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE item_id=NEW.item_id AND provider=(SELECT CASE provider WHEN 'musicbrainz' THEN 'coverartarchive' ELSE provider||'-artwork' END FROM provider_records WHERE id=NEW.record_id);
 UPDATE enrichment_jobs SET request_generation=request_generation+1,state=CASE WHEN state='done' THEN 'pending' ELSE state END,due_at=CASE WHEN state='done' THEN 0 ELSE due_at END WHERE item_id=NEW.item_id AND provider='fanart'
 AND EXISTS(SELECT 1 FROM provider_records WHERE id=NEW.record_id AND ((provider='tvdb' AND namespace='show') OR (provider='tmdb' AND namespace='movie')));
END;

INSERT INTO enrichment_jobs(item_id,provider,revision)
 SELECT i.id,l.provider,i.enrichment_revision FROM collection_items i JOIN collections c ON c.id=i.collection_id
 JOIN metadata_assignments a ON a.item_id=i.id
 JOIN (SELECT media_type,provider FROM enrichment_lanes UNION SELECT media_type,provider FROM provider_order) l ON l.media_type=c.media_type
 WHERE c.media_type IN ('movies','series','anime')
 AND EXISTS(SELECT 1 FROM library_collections lc WHERE lc.collection_id=i.collection_id)
 AND EXISTS(SELECT 1 FROM media_entries e JOIN media_parts p ON p.entry_id=e.id WHERE e.item_id=i.id)
 AND (l.provider IN ('local','local-artwork','tmdb-artwork','tvdb-artwork','anilist-artwork','anidb-artwork','fanart','fanart-artwork')
 OR (l.provider IN ('tmdb','tvdb','anilist') AND (
 EXISTS(SELECT 1 FROM provider_records p WHERE p.id=a.record_id AND p.provider=l.provider)
 OR EXISTS(SELECT 1 FROM provider_links p WHERE p.record_id=a.record_id AND p.provider=l.provider)
 OR EXISTS(SELECT 1 FROM metadata_supplements p JOIN provider_records r ON r.id=p.record_id WHERE p.item_id=i.id AND p.primary_record_id=a.record_id AND r.provider=l.provider))))
 ON CONFLICT(item_id,provider) DO UPDATE SET
 request_generation=enrichment_jobs.request_generation+1,
 state=CASE WHEN enrichment_jobs.state='done' THEN 'pending' ELSE enrichment_jobs.state END,
 due_at=CASE WHEN enrichment_jobs.state='done' THEN 0 ELSE enrichment_jobs.due_at END;
