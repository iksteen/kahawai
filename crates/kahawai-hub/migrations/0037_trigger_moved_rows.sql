-- Close a hole in 0035/0036: an AFTER UPDATE trigger keyed on NEW does
-- not fix up the row's OLD owner, so MOVING a row between items left the
-- one it left behind stale.
--
-- Nothing in the hub moves a row that way — item_id is a primary key in
-- item_match and part of one in provider_metadata and item_sources — but
-- "nothing does that today" is precisely the reasoning that let
-- merged_metadata drift, and the point of maintaining these in the
-- database is that no argument about call sites is required.
--
-- Each replacement recomputes BOTH ends, and item_sources /
-- library_collections gain the UPDATE triggers they never had.

DROP TRIGGER item_match_sort_title_upd;
CREATE TRIGGER item_match_sort_title_upd AFTER UPDATE ON item_match BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = items.id AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id IN (OLD.item_id, NEW.item_id);
END;

DROP TRIGGER provider_metadata_sort_title_upd;
CREATE TRIGGER provider_metadata_sort_title_upd AFTER UPDATE ON provider_metadata BEGIN
    UPDATE items SET sort_title = COALESCE(
        (SELECT pm.title FROM item_match im
           JOIN provider_metadata pm
             ON pm.item_id = im.item_id AND pm.provider = im.provider
          WHERE im.item_id = items.id AND NULLIF(pm.title, '') IS NOT NULL),
        title)
     WHERE id IN (OLD.item_id, NEW.item_id);
END;

-- Membership: a source moving collection, or to another item, changes
-- which libraries hold which item at both ends.
CREATE TRIGGER item_sources_libraries_upd AFTER UPDATE ON item_sources BEGIN
    DELETE FROM item_libraries
     WHERE item_id IN (
        COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id),
        COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id));
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id)
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE COALESCE(ci.parent_id, ci.id) IN (
        COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id),
        COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id));
END;

CREATE TRIGGER library_collections_libraries_upd AFTER UPDATE ON library_collections BEGIN
    DELETE FROM item_libraries WHERE library_id IN (OLD.library_id, NEW.library_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id)
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE lc.library_id IN (OLD.library_id, NEW.library_id);
END;
