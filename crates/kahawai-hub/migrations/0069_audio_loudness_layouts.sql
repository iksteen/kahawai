ALTER TABLE audio_loudness ADD COLUMN source_channel_mask INTEGER;

CREATE TABLE audio_loudness_layouts (
    file_id          INTEGER NOT NULL,
    stream_index     INTEGER NOT NULL,
    channels         INTEGER NOT NULL,
    channel_mask     INTEGER NOT NULL,
    integrated_lufs  REAL NOT NULL,
    true_peak_dbtp   REAL NOT NULL,
    PRIMARY KEY (file_id, stream_index, channels, channel_mask),
    FOREIGN KEY (file_id, stream_index)
        REFERENCES audio_loudness(file_id, stream_index) ON DELETE CASCADE
) WITHOUT ROWID;
