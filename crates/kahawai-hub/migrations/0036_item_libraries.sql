-- NFR-1: which items a library holds, stored rather than re-derived.
--
-- Membership flows up from sources (an item is in a library if any of its
-- own or its children's files sit in one of that library's collections),
-- which meant every browse page and every count re-ran a three-way join
-- over the whole catalogue: 111 ms a page and 105 ms a count at 50k
-- items, against 0.2 ms and 1.9 ms reading this table.
--
-- Like `items.sort_title` (0035), it is maintained by TRIGGERS rather
-- than by remembering to call something. The rule against storing what a
-- read can derive exists because `merged_metadata` drifted; a value the
-- database recomputes on every write to its inputs cannot.
--
-- Each trigger RECOMPUTES the affected item wholesale — delete its rows,
-- re-derive them — rather than trying to apply a delta. An item can reach
-- one library through several collections, so "a source went away" does
-- not mean "it left the library", and a wrong delta is exactly the class
-- of bug this table is meant to avoid.

CREATE TABLE item_libraries (
    library_id TEXT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    item_id    TEXT NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    PRIMARY KEY (library_id, item_id)
) WITHOUT ROWID;

-- Filtering a page asks "is THIS item in that library"; the primary key
-- serves it. Counting a library is a scan of one key range.
CREATE INDEX item_libraries_item ON item_libraries (item_id);

INSERT OR IGNORE INTO item_libraries (library_id, item_id)
SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id)
  FROM library_collections lc
  JOIN item_sources ls
    ON ls.module_id = lc.module_id AND ls.collection_id = lc.collection_id
  JOIN items ci ON ci.id = ls.item_id;

-- A source arriving or leaving changes its TOP-LEVEL item's membership.
CREATE TRIGGER item_sources_libraries_ins AFTER INSERT ON item_sources BEGIN
    DELETE FROM item_libraries
     WHERE item_id = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id)
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE COALESCE(ci.parent_id, ci.id)
           = COALESCE((SELECT parent_id FROM items WHERE id = NEW.item_id), NEW.item_id);
END;

CREATE TRIGGER item_sources_libraries_del AFTER DELETE ON item_sources BEGIN
    DELETE FROM item_libraries
     WHERE item_id = COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id)
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE COALESCE(ci.parent_id, ci.id)
           = COALESCE((SELECT parent_id FROM items WHERE id = OLD.item_id), OLD.item_id);
END;

-- Composing a library is an admin action and rare, so these recompute
-- every item of the collection involved.
CREATE TRIGGER library_collections_libraries_ins AFTER INSERT ON library_collections BEGIN
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT NEW.library_id, COALESCE(ci.parent_id, ci.id)
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
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id)
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE lc.library_id = OLD.library_id;
END;

-- An episode acquiring or changing a parent moves the membership from
-- the episode's own row to the show's.
CREATE TRIGGER items_libraries_parent AFTER UPDATE OF parent_id ON items BEGIN
    DELETE FROM item_libraries WHERE item_id IN (NEW.id, OLD.parent_id, NEW.parent_id);
    INSERT OR IGNORE INTO item_libraries (library_id, item_id)
    SELECT DISTINCT lc.library_id, COALESCE(ci.parent_id, ci.id)
      FROM item_sources ls
      JOIN items ci ON ci.id = ls.item_id
      JOIN library_collections lc
        ON lc.module_id = ls.module_id AND lc.collection_id = ls.collection_id
     WHERE COALESCE(ci.parent_id, ci.id) IN (NEW.id, OLD.parent_id, NEW.parent_id);
END;
