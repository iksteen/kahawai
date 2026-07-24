-- Content-identity lookups: ed2k copy-forward (MH-9) resolves donors by
-- identity; without this the correlated subquery walks files² (13s+).
CREATE INDEX idx_files_identity ON files (size, head_xxh3, tail_xxh3, oshash);
