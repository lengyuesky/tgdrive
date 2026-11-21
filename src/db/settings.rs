//! 键值设置。

use super::*;

impl Db {
    /// 一致性快照备份(WAL 下直接拷 .db 文件会缺最新写入,VACUUM INTO 不会)。
    /// 目标路径由服务端自行生成,不接受用户输入。
    pub async fn vacuum_into(&self, dest: &str) -> AppResult<()> {
        let sql = format!("VACUUM INTO '{}'", dest.replace('\'', "''"));
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn setting_get(&self, k: &str) -> AppResult<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT v FROM settings WHERE k = ?")
            .bind(k)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    /// 一次读取所需设置，避免开关读取混入规则和大体积收件死信。
    pub async fn settings_get(&self, keys: &[&str]) -> AppResult<HashMap<String, String>> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }
        let mut query =
            sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT k, v FROM settings WHERE k IN (");
        let mut separated = query.separated(", ");
        for key in keys {
            separated.push_bind(key);
        }
        separated.push_unseparated(")");
        let rows: Vec<(String, String)> = query.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn setting_set(&self, k: &str, v: &str) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO settings (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        )
        .bind(k)
        .bind(v)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn setting_del(&self, k: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM settings WHERE k = ?")
            .bind(k)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 原子批量更新设置，避免运行时仍是旧值而数据库只写入了一半新配置。
    pub async fn settings_apply(
        &self,
        sets: &[(String, String)],
        deletes: &[String],
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for key in deletes {
            sqlx::query("DELETE FROM settings WHERE k = ?")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        for (key, value) in sets {
            sqlx::query(
                "INSERT INTO settings (k, v) VALUES (?, ?)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 取某前缀下的全部设置(键已去掉前缀)
    pub async fn settings_with_prefix(&self, prefix: &str) -> AppResult<HashMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT k, v FROM settings WHERE k LIKE ? || '%'")
                .bind(prefix)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(k, v)| (k.trim_start_matches(prefix).to_string(), v))
            .collect())
    }
}
