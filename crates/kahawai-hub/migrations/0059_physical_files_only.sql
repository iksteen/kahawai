-- Logical ownership and part order now live only in explicit playable-source
-- rows. Drop the compatibility columns in place: rebuilding `files` would
-- execute dependent ON DELETE CASCADE actions for subtitle/failure rows.
DROP TRIGGER files_playable_insert;
DROP TRIGGER files_playable_rebind;
DROP TRIGGER files_collection_refs_ins;
DROP TRIGGER files_collection_refs_upd;
DROP INDEX files_v53_item;

ALTER TABLE files DROP COLUMN item_id;
ALTER TABLE files DROP COLUMN part;

CREATE TRIGGER files_collection_refs_ins BEFORE INSERT ON files
WHEN NEW.root_id IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM collection_roots r WHERE r.id=NEW.root_id
      AND (r.module_id,r.collection_id)=(NEW.module_id,NEW.collection_id))
BEGIN
    SELECT RAISE(ABORT,'file root belongs to another collection');
END;
CREATE TRIGGER files_collection_refs_upd
BEFORE UPDATE OF module_id,collection_id,root_id ON files
WHEN NEW.root_id IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM collection_roots r WHERE r.id=NEW.root_id
      AND (r.module_id,r.collection_id)=(NEW.module_id,NEW.collection_id))
BEGIN
    SELECT RAISE(ABORT,'file root belongs to another collection');
END;

ANALYZE;
