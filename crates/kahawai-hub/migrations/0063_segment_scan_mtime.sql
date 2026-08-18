-- A scan record is about BYTES, not about a name.
--
-- Without this a replaced file kept its episode's "already analysed" row for
-- ever: re-download a truncated download, re-encode a season, restore from a
-- backup, and the boundaries stay whatever was found in the old bytes — or
-- stay absent, if the old bytes were the reason nothing was found. The
-- subtitle side answered the same question the same way in 0048.
--
-- Existing rows are backfilled with what their files say now, which is true of
-- every scan that has actually happened: they were analysed from the bytes
-- currently on disk. A row for an item with no files stays NULL and is treated
-- as matching, so a missing value can never become a re-analysis loop.
ALTER TABLE media_segment_scans ADD COLUMN mtime_unix INTEGER;

UPDATE media_segment_scans
   SET mtime_unix = (
        SELECT MAX(f.mtime_unix)
          FROM playable_sources ps
          JOIN playable_source_parts psp ON psp.playable_source_id = ps.id
          JOIN files f ON f.id = psp.file_id
         WHERE ps.item_id = media_segment_scans.item_id
   );
