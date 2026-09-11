CREATE INDEX library_alias_target ON library_items(merged_into,id) WHERE merged_into IS NOT NULL;
CREATE INDEX library_rejection_target ON rejected_library_matches(library_item_id,collection_item_id);

INSERT INTO library_pending(collection_item_id)
SELECT DISTINCT r.collection_item_id FROM library_items alias
JOIN rejected_library_matches r ON r.library_item_id=alias.id
WHERE alias.merged_into IS NOT NULL
ON CONFLICT DO NOTHING;
