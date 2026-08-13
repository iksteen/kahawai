-- Exact-root adoption rewrites only a source's relational key. Library
-- membership depends on the source's module, collection and item, not its path
-- spelling; firing the broad historical trigger once per adopted row made the
-- lossless upgrade quadratic in a collection's presentation size.
DROP TRIGGER item_sources_libraries_upd;

CREATE TRIGGER item_sources_libraries_upd
AFTER UPDATE OF module_id, collection_id, item_id ON item_sources
WHEN OLD.module_id IS NOT NEW.module_id
  OR OLD.collection_id IS NOT NEW.collection_id
  OR OLD.item_id IS NOT NEW.item_id
BEGIN
    DELETE FROM item_libraries
     WHERE item_id IN (
        COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id),
        COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id));
    INSERT OR IGNORE INTO item_libraries (library_id, item_id, sort_title, year)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id),
           (SELECT sort_title FROM items WHERE id = COALESCE(ci.parent_id, ci.id)),
           (SELECT year FROM items WHERE id = COALESCE(ci.parent_id, ci.id))
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE COALESCE(ci.parent_id, ci.id) IN (
        COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id),
        COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id));
END;
