-- NFR-1/NFR-2: browse a library without touching `items` until the page
-- is chosen.
--
-- A deep page walks OFFSET rows before it keeps any. Each skipped row
-- used to cost an index step on `items` PLUS a membership probe into
-- item_libraries — 250k probes made the last page of a 250k library cost
-- over a second, and no page cache fixes an O(offset) probe chain.
--
-- Carrying the sort keys IN the membership table turns that walk into a
-- pure covering-index scan: skipping a row is one index step, nothing
-- else. Measured on the 250k benchmark: the deep title page went from
-- 1222 ms through the endpoint to 21 ms.
--
-- These columns are derived from `items` (which itself derives
-- sort_title from the assigned answer, 0035). They stay true the same
-- way everything derived here does — by triggers, never by callers:
--   * membership rows are created WITH their keys (the rewritten
--     triggers below);
--   * a key change on `items` follows into membership
--     (`item_libraries_sort_keys`) — this chains off 0035's own
--     triggers, which is what keeps a provider retitle flowing all the
--     way through: answer → items.sort_title → item_libraries.
-- `tests/sort_title.rs` re-derives the truth from scratch and fails on
-- any drift, raw SQL included.

ALTER TABLE item_libraries ADD COLUMN sort_title TEXT;
ALTER TABLE item_libraries ADD COLUMN year INTEGER;

UPDATE item_libraries SET
    sort_title = (SELECT i.sort_title FROM items i WHERE i.id = item_id),
    year       = (SELECT i.year FROM items i WHERE i.id = item_id);

-- The browse index: a library's page in title order is a range scan of
-- this alone, at any offset. `item_id` is IN the index, so the scan
-- order is total — page boundaries cannot split a tie differently on
-- two requests, which the bare (sort_title, year) order left to the
-- accident of rowid order.
CREATE INDEX item_libraries_browse
    ON item_libraries (library_id, sort_title, year, item_id);

ANALYZE item_libraries;

-- A sort key changing on items follows into every membership row.
-- The WHEN guard matters: 0035's triggers rewrite items.sort_title
-- unconditionally on every answer write, almost always to the same
-- value, and this must not turn each of those into membership churn.
CREATE TRIGGER item_libraries_sort_keys AFTER UPDATE OF sort_title, year ON items
WHEN NEW.sort_title IS NOT OLD.sort_title OR NEW.year IS NOT OLD.year
BEGIN
    UPDATE item_libraries SET sort_title = NEW.sort_title, year = NEW.year
     WHERE item_id = NEW.id;
END;

-- The membership triggers (0036, made sargable in 0039), now inserting
-- the keys alongside the row. Same statements otherwise; the key
-- subqueries are indexed PK lookups on the projected TOP-LEVEL item.

DROP TRIGGER item_sources_libraries_ins;
DROP TRIGGER item_sources_libraries_del;
DROP TRIGGER library_collections_libraries_ins;
DROP TRIGGER library_collections_libraries_del;
DROP TRIGGER items_libraries_parent;

CREATE TRIGGER item_sources_libraries_ins AFTER INSERT ON item_sources BEGIN
    DELETE FROM item_libraries
     WHERE item_id = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT lc.library_id,
           COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id),
           (SELECT sort_title FROM items
             WHERE id = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id)),
           (SELECT year FROM items
             WHERE id = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id))
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
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT lc.library_id,
           COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id),
           (SELECT sort_title FROM items
             WHERE id = COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id)),
           (SELECT year FROM items
             WHERE id = COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id))
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

CREATE TRIGGER library_collections_libraries_ins AFTER INSERT ON library_collections BEGIN
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT NEW.library_id, COALESCE(ci.parent_id, ci.id),
           (SELECT sort_title FROM items WHERE id = COALESCE(ci.parent_id, ci.id)),
           (SELECT year FROM items WHERE id = COALESCE(ci.parent_id, ci.id))
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
     WHERE ls.module_id = NEW.module_id AND ls.collection_id = NEW.collection_id;
END;

CREATE TRIGGER library_collections_libraries_del AFTER DELETE ON library_collections BEGIN
    -- Drop the library's claim on those items, then re-add whatever its
    -- REMAINING collections still justify.
    DELETE FROM item_libraries
     WHERE library_id = OLD.library_id
       AND item_id IN (SELECT DISTINCT COALESCE(ci.parent_id, ci.id)
                         FROM item_sources ls
                         JOIN items ci ON ci.id = ls.item_id
                        WHERE ls.module_id = OLD.module_id
                          AND ls.collection_id = OLD.collection_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id),
           (SELECT sort_title FROM items WHERE id = COALESCE(ci.parent_id, ci.id)),
           (SELECT year FROM items WHERE id = COALESCE(ci.parent_id, ci.id))
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE lc.library_id = OLD.library_id;
END;

CREATE TRIGGER items_libraries_parent AFTER UPDATE OF parent_id ON items BEGIN
    DELETE FROM item_libraries WHERE item_id IN (NEW.id, OLD.parent_id, NEW.parent_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id),
           (SELECT sort_title FROM items WHERE id = COALESCE(ci.parent_id, ci.id)),
           (SELECT year FROM items WHERE id = COALESCE(ci.parent_id, ci.id))
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE COALESCE(ci.parent_id, ci.id) IN (NEW.id, OLD.parent_id, NEW.parent_id);
END;
