-- Tracks keep a played mark, never a resume position.
--
-- Continue-watching is driven off `watch_state` and filters `position_ms > 0
-- AND played = 0`, then discards tracks by kind AFTER joining `items`. So every
-- track ever skipped part-way sat in the scanned set for good, and nothing
-- pruned them: a shuffle listener accumulates tens of thousands and the home
-- page pays for all of them, twice, on every load.
--
-- `post_progress` now stores zero for a track. This clears what was written
-- before it did. Only the position is zeroed — `played` and `play_count` are
-- what the album page renders per row, and they stay exactly as they are. A
-- record is resumed from its place in the queue, never from a stored offset,
-- so nothing reads the value being cleared.
UPDATE watch_state
   SET position_ms = 0
 WHERE position_ms <> 0
   AND item_id IN (SELECT id FROM items WHERE kind = 'track');
