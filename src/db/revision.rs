//! 用户数据修订号：由数据库触发器维护，供备份判断数据是否变化。

use super::Db;
use crate::error::AppResult;

impl Db {
    pub async fn data_revision(&self) -> AppResult<i64> {
        Ok(
            sqlx::query_scalar("SELECT revision FROM data_revision WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::db::{Db, ROOT_ID};

    async fn raw_setting(db: &Db, key: &str) {
        sqlx::query("INSERT INTO settings (k, v) VALUES (?, 'x')")
            .bind(key)
            .execute(&db.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn data_revision_ignores_runtime_settings_and_bumps_on_substantive_writes() {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(directory.path().join("revision.db").to_str().unwrap())
            .await
            .unwrap();

        // ---------- 排除侧：运行时键的写入与删除不改变修订号 ----------
        let noise = [
            "extra.backup_telegram_last_ts",
            "extra.backup_webdav_last_ts",
            "extra.backup_s3_last_ts",
            "extra.backup_last_ts",
            "extra.backup_msg_groups",
            "extra.backup_msg_ids",
            "extra.backup_webdav_history",
            "extra.backup_s3_history",
            "extra.backup_telegram_data_revision",
            "extra.backup_webdav_data_revision",
            "extra.backup_s3_data_revision",
            "extra.inbox_offset",
            "extra.seen_chats",
            "extra.inbox_failure.123",
            "mtproto.session.7",
        ];
        for key in noise {
            let before = db.data_revision().await.unwrap();
            db.setting_set(key, "1").await.unwrap();
            db.setting_set(key, "2").await.unwrap();
            db.setting_del(key).await.unwrap();
            // 收件死信走的是裸 SQL 插入，覆盖 INSERT 触发器的非 UPSERT 路径。
            if key == "extra.inbox_failure.123" || key == "mtproto.session.7" {
                raw_setting(&db, key).await;
                db.setting_del(key).await.unwrap();
            }
            assert_eq!(
                db.data_revision().await.unwrap(),
                before,
                "运行时键 {key} 不应改变修订号"
            );
        }

        // ---------- 触发侧：逐步精确计数，某个触发器漏建时立刻失败 ----------
        let mut expected = db.data_revision().await.unwrap();

        // settings：实质配置 INSERT/UPDATE/DELETE 各 +1
        db.setting_set("extra.trash_retention_days", "7").await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);
        // 同值 UPSERT 不触发（OLD.v IS NOT NEW.v 守卫）。
        db.setting_set("extra.trash_retention_days", "7").await.unwrap();
        assert_eq!(db.data_revision().await.unwrap(), expected);
        db.setting_set("extra.trash_retention_days", "8").await.unwrap();
        expected += 1;
        db.setting_del("extra.trash_retention_days").await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);

        db.setting_set("cfg.chat_id", "-100").await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);

        // nodes + chunks：创建文件 nodes INSERT + chunks INSERT。
        // 先建一个存储目标（storage_targets INSERT，+1），chunks.storage_target_id 的外键指向它。
        let target = db
            .ensure_storage_target("https://api.example.invalid", "1:rev", "-1")
            .await
            .unwrap()
            .id;
        expected += 1;
        let id = db.create_uploading(ROOT_ID, "a.bin").await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);
        db.add_chunk(id, 0, 4, "file_id", 100, target, None)
            .await
            .unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);
        // finalize 会 UPDATE nodes（改尺寸/状态）。
        db.finalize_upload(id, 4, None).await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);

        // nodes UPDATE：收藏切换。
        db.set_favorite(id, true).await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);

        // shares：INSERT +1；领取下载只加 downloads 计数器，不触发；DELETE +1。
        let share = db.share_create(id, None, None, None).await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);
        sqlx::query("UPDATE shares SET downloads = downloads + 1 WHERE id = ?")
            .bind(share.id)
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            db.data_revision().await.unwrap(),
            expected,
            "领取下载只更新计数器，不应改变修订号"
        );
        db.share_delete(share.id).await.unwrap();
        expected += 1;
        assert_eq!(db.data_revision().await.unwrap(), expected);

        // 删除节点：nodes DELETE + chunks DELETE。
        db.delete_node(id).await.unwrap();
        expected += 2;
        assert_eq!(db.data_revision().await.unwrap(), expected);
    }
}
