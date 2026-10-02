CREATE TABLE subtitle_jobs_v3 (
 file_id TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
 kind TEXT NOT NULL CHECK(kind IN ('text','sets')),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','running','retry','blocked')),
 due_at INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0,
 token TEXT, lease_until INTEGER NOT NULL DEFAULT 0,
 host TEXT, error TEXT,
 PRIMARY KEY(file_id,kind)
);
INSERT INTO subtitle_jobs_v3 SELECT * FROM subtitle_jobs WHERE state <> 'done';
DROP TABLE subtitle_jobs;
ALTER TABLE subtitle_jobs_v3 RENAME TO subtitle_jobs;
CREATE INDEX subtitle_jobs_due ON subtitle_jobs(kind,state,due_at,lease_until);
