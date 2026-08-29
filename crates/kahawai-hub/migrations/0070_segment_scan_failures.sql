ALTER TABLE media_segment_scans ADD COLUMN module_id TEXT;
ALTER TABLE media_segment_scans ADD COLUMN collection_id TEXT;
ALTER TABLE media_segment_scans ADD COLUMN root_token TEXT;
ALTER TABLE media_segment_scans ADD COLUMN path_rel TEXT;
ALTER TABLE media_segment_scans ADD COLUMN size INTEGER;
ALTER TABLE media_segment_scans ADD COLUMN error TEXT NOT NULL DEFAULT '';
