INSERT INTO library_items(id,kind,title,norm_title,sort_title,unidentified,merged_into,added_id)
SELECT i.id,target.kind,target.title,target.norm_title,target.sort_title,1,target.id,i.id
FROM collection_items i
JOIN collection_item_library_items a ON a.collection_item_id=i.id AND a.ordinal=1
JOIN library_items target ON target.id=a.library_item_id
WHERE NOT EXISTS(SELECT 1 FROM library_items existing WHERE existing.id=i.id);

INSERT INTO library_items(id,kind,title,norm_title,sort_title,unidentified,added_id)
SELECT i.id,CASE i.kind WHEN 'show' THEN 'series' WHEN 'track' THEN 'song' ELSE i.kind END,
       i.title,i.norm_title,i.sort_title,1,i.id
FROM collection_items i
WHERE NOT EXISTS(SELECT 1 FROM library_items existing WHERE existing.id=i.id);

INSERT INTO collection_item_library_items(collection_item_id,ordinal,library_item_id)
SELECT i.id,1,i.id FROM collection_items i JOIN library_items li ON li.id=i.id
WHERE li.unidentified=1 AND li.merged_into IS NULL
  AND NOT EXISTS(SELECT 1 FROM collection_item_library_items a WHERE a.collection_item_id=i.id);

WITH affected AS (
    SELECT child.id FROM collection_items child
    JOIN collection_items parent ON parent.id=child.parent_id
    WHERE parent.metadata_eligible=0
)
INSERT INTO library_pending(collection_item_id)
SELECT id FROM affected
UNION
SELECT sibling.collection_item_id FROM collection_item_library_items sibling
WHERE sibling.library_item_id IN(
    SELECT a.library_item_id FROM collection_item_library_items a JOIN affected ON affected.id=a.collection_item_id
)
ON CONFLICT DO NOTHING;
