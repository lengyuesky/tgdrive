-- 整体清除旧应用平台；只删除应用对象，不修改网盘本体的任何数据行。
-- 先撤销应用触发器，避免删表时的级联删除影响网盘的数据修订号。
DROP TRIGGER IF EXISTS data_revision_apps_insert;
DROP TRIGGER IF EXISTS data_revision_apps_delete;
DROP TRIGGER IF EXISTS data_revision_apps_update;
DROP TRIGGER IF EXISTS data_revision_app_settings_insert;
DROP TRIGGER IF EXISTS data_revision_app_settings_delete;
DROP TRIGGER IF EXISTS data_revision_app_settings_update;
DROP TRIGGER IF EXISTS data_revision_app_data_insert;
DROP TRIGGER IF EXISTS data_revision_app_data_delete;
DROP TRIGGER IF EXISTS data_revision_app_data_update;
DROP TRIGGER IF EXISTS data_revision_app_recovery_insert;
DROP TRIGGER IF EXISTS data_revision_app_recovery_delete;
DROP TRIGGER IF EXISTS data_revision_app_recovery_update;
DROP TRIGGER IF EXISTS data_revision_app_lifecycle_insert;
DROP TRIGGER IF EXISTS data_revision_app_lifecycle_delete;
DROP TRIGGER IF EXISTS data_revision_app_lifecycle_update;

DROP TRIGGER IF EXISTS reading_tree_insert;
DROP TRIGGER IF EXISTS reading_tree_delete;
DROP TRIGGER IF EXISTS reading_tree_update;
DROP VIEW IF EXISTS reading_files;
DROP INDEX IF EXISTS idx_reading_format;
DROP TABLE IF EXISTS reading_tree_revision;

DROP INDEX IF EXISTS app_scraper_one_active;
DROP INDEX IF EXISTS app_scraper_history;
DROP INDEX IF EXISTS app_scraper_items_next;
DROP TABLE IF EXISTS app_scraper_upload_guards;
DROP TABLE IF EXISTS app_scraper_sources;
DROP TABLE IF EXISTS app_scraper_items;
DROP TABLE IF EXISTS app_scraper_directories;
DROP TABLE IF EXISTS app_scraper_batches;
DROP TABLE IF EXISTS app_scraper_state;

DROP INDEX IF EXISTS idx_app_data_recent;
DROP TABLE IF EXISTS app_recovery;
DROP TABLE IF EXISTS app_lifecycle;
DROP TABLE IF EXISTS app_cleanup_jobs;
DROP TABLE IF EXISTS app_settings;
DROP TABLE IF EXISTS app_data;
DROP TABLE IF EXISTS apps;
