CREATE TABLE media_segment_failures (
    item_id       TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    module_id     TEXT NOT NULL,
    collection_id TEXT NOT NULL,
    root_token    TEXT NOT NULL,
    path_rel      TEXT NOT NULL,
    size          INTEGER NOT NULL,
    mtime_unix    INTEGER NOT NULL,
    detector      INTEGER NOT NULL,
    error         TEXT NOT NULL,
    failed_at     INTEGER NOT NULL,
    PRIMARY KEY (
        item_id, module_id, collection_id, root_token,
        path_rel, size, mtime_unix, detector
    )
) WITHOUT ROWID;

INSERT INTO media_segment_failures
       (item_id,module_id,collection_id,root_token,path_rel,size,mtime_unix,
        detector,error,failed_at)
SELECT item_id,module_id,collection_id,root_token,path_rel,size,mtime_unix,
       detector,error,scanned_at
  FROM media_segment_scans
 WHERE error != '' AND module_id IS NOT NULL AND collection_id IS NOT NULL
   AND root_token IS NOT NULL AND path_rel IS NOT NULL AND size IS NOT NULL;

DELETE FROM media_segment_scans WHERE error != '';
