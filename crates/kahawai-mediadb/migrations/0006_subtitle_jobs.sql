CREATE TABLE subtitle_jobs (
 file_id TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 kind TEXT NOT NULL CHECK(kind IN ('text','ocr')),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','running','retry','blocked','done')),
 due_at INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0,
 token TEXT, lease_until INTEGER NOT NULL DEFAULT 0,
 host TEXT, error TEXT,
 PRIMARY KEY(file_id,kind)
);
CREATE INDEX subtitle_jobs_due ON subtitle_jobs(kind,state,due_at,lease_until);
INSERT INTO subtitle_jobs(file_id,kind) SELECT id,'text' FROM files WHERE media_json IS NOT NULL;
INSERT INTO subtitle_jobs(file_id,kind) SELECT id,'ocr' FROM files WHERE media_json IS NOT NULL;
