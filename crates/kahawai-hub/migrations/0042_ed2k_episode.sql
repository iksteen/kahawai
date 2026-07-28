-- HUB-30: the FILE reply names the EPISODE, so keep it.
--
-- `ed2k_aid` caches AniDB's answer per content hash — ask-once, forever.
-- Until now it kept only the aid, although the reply always carried the
-- episode identity too: the hub fetched "this file IS episode S1 of
-- anime X, released by group Y" and stored "belongs to anime X".
--
-- These columns complete the cached answer. `epno` is AniDB's own
-- episode string ("1", "01", "S1" special, "C1" credit, "T1" trailer,
-- "P"/"O" parody/other); it is aid-scoped, which is why the binder in
-- enrich.rs only acts on files whose aid matches their show's. A row
-- with an aid but NULL epno predates this migration and is re-asked
-- once; a NULL aid is a recorded miss and stays terminal.
ALTER TABLE ed2k_aid ADD COLUMN eid INTEGER;
ALTER TABLE ed2k_aid ADD COLUMN epno TEXT;
ALTER TABLE ed2k_aid ADD COLUMN gid INTEGER;
ALTER TABLE ed2k_aid ADD COLUMN group_name TEXT;
