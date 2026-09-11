ALTER TABLE provider_metadata ADD COLUMN parent_library_item_id TEXT REFERENCES library_items(id);
ALTER TABLE collection_items ADD COLUMN provider_identity_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE provider_metadata ADD COLUMN identity_revision INTEGER;
CREATE INDEX provider_answer_parent ON provider_metadata(parent_library_item_id) WHERE parent_library_item_id IS NOT NULL;

INSERT INTO library_pending(collection_item_id)
SELECT i.id FROM collection_items i
JOIN collection_item_library_items a ON a.collection_item_id=i.id AND a.ordinal=1
JOIN library_items l ON l.id=a.library_item_id
WHERE i.kind IN ('episode','track') AND i.metadata_eligible=1 AND i.assignment_manual=1
ON CONFLICT DO NOTHING;
