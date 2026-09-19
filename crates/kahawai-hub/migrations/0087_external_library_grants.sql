CREATE TABLE user_libraries_external (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    library_id TEXT NOT NULL,
    PRIMARY KEY(user_id, library_id)
) WITHOUT ROWID;
INSERT INTO user_libraries_external SELECT user_id,library_id FROM user_libraries;
DROP TABLE user_libraries;
ALTER TABLE user_libraries_external RENAME TO user_libraries;
