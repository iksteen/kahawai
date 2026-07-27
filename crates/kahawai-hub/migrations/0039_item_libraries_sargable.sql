-- NFR-1: make library membership cost O(1) per source instead of O(n).
--
-- 0036's triggers select the affected item with
-- `WHERE COALESCE(ci.parent_id, ci.id) = <the item>`. That is an
-- expression over a joined row, not a column, so no index can serve it:
-- SQLite scanned all of `item_sources` joined to `items` for EVERY source
-- inserted or deleted. A scan inserts sources in bulk, which made the
-- hottest write path in the system quadratic in catalogue size —
-- measured at 0.05 ms per row with no library configured and 4.5 ms with
-- one at 16k items, still climbing.
--
-- The set is the same either way: the sources of the top-level item, plus
-- the sources of its children. Saying that directly makes both halves
-- index lookups (`item_sources_item`, then `items_children`), and the
-- membership rows produced are identical — the projected id is the
-- top-level item, which is what the predicate had already pinned it to.
--
-- `items_libraries_parent` has the same shape and is left alone: it fires
-- on UPDATE OF parent_id, and nothing updates parent_id — items are
-- inserted with the parent they have.

DROP TRIGGER item_sources_libraries_ins;
DROP TRIGGER item_sources_libraries_del;

CREATE TRIGGER item_sources_libraries_ins AFTER INSERT ON item_sources BEGIN
    DELETE FROM item_libraries
     WHERE item_id = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id,
           COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id)
      FROM item_sources ls
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE ls.item_id
           = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id)
        OR ls.item_id IN (
             SELECT id FROM items
              WHERE parent_id
                    = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id),
                               NEW.item_id));
END;

CREATE TRIGGER item_sources_libraries_del AFTER DELETE ON item_sources BEGIN
    DELETE FROM item_libraries
     WHERE item_id = COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id,
           COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id)
      FROM item_sources ls
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE ls.item_id
           = COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id)
        OR ls.item_id IN (
             SELECT id FROM items
              WHERE parent_id
                    = COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id),
                               OLD.item_id));
END;
