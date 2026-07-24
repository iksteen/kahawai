-- Efficiency ladder step 2: mediahost-side subtitle extraction. Flags
-- files whose embedded text subtitles have been extracted and cached
-- hub-side; reset when content changes (same rule as ed2k).
ALTER TABLE files ADD COLUMN subs_extracted INTEGER NOT NULL DEFAULT 0;
