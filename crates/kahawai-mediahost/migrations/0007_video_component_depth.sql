ALTER TABLE catalog_files ADD COLUMN reprobe_required INTEGER NOT NULL DEFAULT 0;
INSERT INTO catalog_meta(key,value) VALUES('video_component_depth_migration','pending');
