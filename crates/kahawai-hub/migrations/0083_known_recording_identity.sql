INSERT INTO library_pending(collection_item_id)
SELECT a.collection_item_id FROM library_items unresolved
JOIN collection_item_library_items a ON a.library_item_id=unresolved.id
JOIN collection_items copy ON copy.id=a.collection_item_id
WHERE unresolved.kind='song' AND unresolved.unidentified=1 AND unresolved.merged_into IS NULL
  AND unresolved.recording_id IS NOT NULL AND copy.assignment_manual=0
  AND EXISTS(SELECT 1 FROM library_items known WHERE known.kind='song'
      AND known.recording_id=unresolved.recording_id AND known.unidentified=0 AND known.merged_into IS NULL)
ON CONFLICT DO NOTHING;
