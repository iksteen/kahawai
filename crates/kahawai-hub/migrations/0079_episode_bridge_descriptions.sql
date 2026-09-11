INSERT INTO library_pending(collection_item_id)
SELECT DISTINCT i.id FROM collection_items i
JOIN item_match parent ON parent.item_id=i.parent_id
JOIN provider_metadata pm ON pm.item_id=i.id AND pm.provider<>parent.provider
WHERE i.kind='episode' AND i.assignment_manual=0
  AND NULLIF(trim(pm.title),'') IS NOT NULL AND pm.confidence<>'weak'
  AND NOT EXISTS(SELECT 1 FROM item_match own WHERE own.item_id=i.id)
  AND NOT EXISTS(SELECT 1 FROM rejected_matches rj
    WHERE rj.item_id=pm.item_id AND rj.provider=pm.provider AND rj.provider_id=pm.provider_id)
ON CONFLICT DO NOTHING;
