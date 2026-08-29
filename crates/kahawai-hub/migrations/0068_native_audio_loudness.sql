ALTER TABLE audio_loudness ADD COLUMN source_channels INTEGER;
ALTER TABLE audio_loudness ADD COLUMN native_integrated_lufs REAL;
ALTER TABLE audio_loudness ADD COLUMN native_true_peak_dbtp REAL;
