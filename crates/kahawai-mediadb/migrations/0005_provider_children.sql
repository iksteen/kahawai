ALTER TABLE provider_records ADD COLUMN children_json TEXT;

CREATE TEMP TABLE child_catalogue_upgrade (old_json TEXT PRIMARY KEY, new_json TEXT);
INSERT INTO child_catalogue_upgrade(old_json)
SELECT json_extract(description_json,'$.children') FROM provider_records
 WHERE json_type(description_json,'$.children')='array'
UNION
SELECT json_extract(candidate.value,'$.record.description.children')
 FROM enrichment_cache, json_each(answer,'$.candidates') candidate
 WHERE json_type(candidate.value,'$.record.description.children')='array'
UNION
SELECT json_extract(answer,'$.local.description.children') FROM enrichment_cache
 WHERE json_type(answer,'$.local.description.children')='array';

UPDATE child_catalogue_upgrade SET new_json=(
 SELECT json_group_array(json_object(
  'provider_id',json_extract(value,'$.provider_id'),
  'title',json_extract(value,'$.title'),
  'position',json_object(
   'season',json_extract(value,'$.season'), 'episode',json_extract(value,'$.episode'),
   'absolute',json_extract(value,'$.absolute'), 'disc',json_extract(value,'$.disc'),
   'track',json_extract(value,'$.track')),
  'description',json_object(
   'overview',json_extract(value,'$.overview'), 'rating',json_extract(value,'$.rating'),
   'release_date',json_extract(value,'$.release_date'),
   'artwork',CASE WHEN json_type(value,'$.artwork')='text'
                  THEN json_array(json_extract(value,'$.artwork')) ELSE NULL END)
 )) FROM json_each(old_json)
);

DROP TRIGGER enrichment_description_art;
UPDATE provider_records SET
 children_json=(SELECT new_json FROM child_catalogue_upgrade WHERE old_json=json_extract(description_json,'$.children')),
 description_json=json_remove(description_json,'$.children');
CREATE TRIGGER enrichment_description_art AFTER UPDATE OF description_json ON provider_records
WHEN NEW.description_json<>OLD.description_json BEGIN
 UPDATE enrichment_jobs SET state='pending',due_at=0 WHERE provider IN ('tmdb-artwork','tvdb-artwork','anilist-artwork','local-artwork','coverartarchive','artist-collage')
 AND (item_id IN (SELECT item_id FROM metadata_assignments WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM metadata_supplements WHERE record_id=NEW.id)
 OR item_id IN (SELECT item_id FROM local_metadata WHERE record_id=NEW.id));
END;

UPDATE enrichment_cache SET answer=json_set(answer,'$.candidates',json((
 SELECT json_group_array(json_set(candidate.value,
  '$.record.children',json((SELECT new_json FROM child_catalogue_upgrade
   WHERE old_json=json_extract(candidate.value,'$.record.description.children'))),
  '$.record.description',json_remove(json_extract(candidate.value,'$.record.description'),'$.children')
 )) FROM json_each(answer,'$.candidates') candidate
))) WHERE json_type(answer,'$.candidates')='array';
UPDATE enrichment_cache SET answer=json_set(answer,
 '$.local.children',json((SELECT new_json FROM child_catalogue_upgrade
  WHERE old_json=json_extract(answer,'$.local.description.children'))),
 '$.local.description',json_remove(json_extract(answer,'$.local.description'),'$.children')
) WHERE json_type(answer,'$.local')='object';
DROP TABLE child_catalogue_upgrade;
