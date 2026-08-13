-- The child-side triggers from migration 54 reject a derivative whose owner
-- differs at INSERT/UPDATE time. Complete the invariant in the other direction:
-- changing a parent's owner must not strand children under the old owner.
CREATE TRIGGER subtitle_parent_owner_upd
BEFORE UPDATE OF item_id,source_id ON subtitle_tracks
WHEN EXISTS(
    SELECT 1 FROM subtitle_tracks child
     WHERE child.derived_from=OLD.id
       AND (child.item_id IS NOT NEW.item_id OR child.source_id IS NOT NEW.source_id))
BEGIN
    SELECT RAISE(ABORT,'subtitle parent has derivatives with another owner');
END;
