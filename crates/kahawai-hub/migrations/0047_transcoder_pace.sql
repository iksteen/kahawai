CREATE TABLE IF NOT EXISTS transcoder_pace (
    module_id   TEXT    NOT NULL,
    work_class  TEXT    NOT NULL,
    multiple    REAL    NOT NULL,
    samples     INTEGER NOT NULL DEFAULT 1,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (module_id, work_class)
) WITHOUT ROWID;
