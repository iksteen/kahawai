DROP VIEW IF EXISTS library_membership;
CREATE VIEW library_membership AS
SELECT a.library_item_id AS item_id,i.module_id,i.collection_id,lc.library_id
FROM collection_item_library_items a JOIN collection_items i INDEXED BY catalog_copy_membership ON i.id=a.collection_item_id
JOIN library_collections lc ON (lc.module_id,lc.collection_id)=(i.module_id,i.collection_id);

DROP VIEW IF EXISTS library_sources;
CREATE VIEW library_sources AS
SELECT a.library_item_id AS item_id,ps.id,ps.item_id AS collection_item_id,
       ps.module_id,ps.collection_id,ps.root_id,ps.family_key,ps.expected_parts,a.ordinal,
       ia.assignment_revision
FROM collection_item_library_items a JOIN playable_sources ps ON ps.item_id=a.collection_item_id
JOIN collection_items ia ON ia.id=a.collection_item_id;

DROP VIEW IF EXISTS library_entries;
CREATE VIEW library_entries AS
SELECT c.*,
 (SELECT series_id FROM episode_details e WHERE e.item_id=c.id) AS parent_id,
 (SELECT season FROM episode_details e WHERE e.item_id=c.id) AS season,
 (SELECT episode FROM episode_details e WHERE e.item_id=c.id) AS episode,
 NULL AS episode_end,
 (SELECT collection_item_id FROM collection_item_library_items a WHERE a.library_item_id=c.id ORDER BY a.collection_item_id LIMIT 1) AS representative_id
FROM library_items c WHERE c.merged_into IS NULL;

DROP VIEW IF EXISTS album_copies;
CREATE VIEW album_copies AS
SELECT ci.id AS collection_item_id,t.* FROM collection_items ci JOIN album_tracks t ON t.id=ci.album_track_id;
