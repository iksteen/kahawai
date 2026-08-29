CREATE TABLE audio_loudness (
    file_id          INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    stream_index     INTEGER NOT NULL,
    analyzer         INTEGER NOT NULL,
    size             INTEGER NOT NULL,
    mtime_unix       INTEGER NOT NULL,
    integrated_lufs  REAL,
    true_peak_dbtp   REAL,
    error            TEXT NOT NULL DEFAULT '',
    measured_at      INTEGER NOT NULL,
    PRIMARY KEY (file_id, stream_index)
);
