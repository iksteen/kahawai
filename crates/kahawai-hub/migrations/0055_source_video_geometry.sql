-- PAR/orientation/display dimensions are physical-source facts. They live in
-- the source's evolving MediaInfo JSON, not on items or library composition.
-- This index keeps the targeted legacy-row worklist bounded without coupling it
-- to scans or reconciliation.
CREATE INDEX files_video_geometry_pending
    ON files(module_id,collection_id,id)
    WHERE json_extract(streams_json,'$.video[0].codec') IS NOT NULL
      AND COALESCE(json_extract(streams_json,'$.video_geometry_probed'),0)=0;
