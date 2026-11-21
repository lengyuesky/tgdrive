//! 后台事件日志。

use super::*;

impl Db {
    /// 写入一条面向管理员的后台事件摘要。
    /// 日志写入失败不应影响主业务，调用方通常只记录 tracing 警告后继续。
    pub async fn append_backend_log(
        &self,
        level: &str,
        category: &str,
        message: &str,
    ) -> AppResult<()> {
        let level = match level {
            "debug" | "info" | "warn" | "error" => level,
            _ => "info",
        };
        let category: String = category.chars().take(80).collect();
        let message: String = message.chars().take(1_000).collect();
        let category = if category.trim().is_empty() {
            "系统"
        } else {
            category.trim()
        };
        let message = if message.trim().is_empty() {
            "后台事件"
        } else {
            message.trim()
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO backend_logs (level, category, message, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(level)
        .bind(category)
        .bind(message)
        .bind(now_ts())
        .execute(&mut *tx)
        .await?;
        // 按自增 id 保留最近固定数量，避免高频后台任务让数据库无限增长。
        sqlx::query(
            "DELETE FROM backend_logs
             WHERE id <= (SELECT COALESCE(MAX(id), 0) - ? FROM backend_logs)",
        )
        .bind(BACKEND_LOG_KEEP)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 按 id 倒序读取后台日志；`before` 用于向前翻页，`filter` 在库内完成筛选。
    ///
    /// 筛选必须走 SQL：日志上限 2000 条且常被同一类事件刷屏，只在前端过滤当前页
    /// 会出现“筛选了错误却一条都看不到”的假象。
    pub async fn backend_logs(
        &self,
        limit: i64,
        before: Option<i64>,
        filter: &BackendLogFilter,
    ) -> AppResult<(Vec<BackendLog>, bool)> {
        let limit = limit.clamp(1, 100);
        let fetch_limit = limit + 1;
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT id, level, category, message, created_at FROM backend_logs WHERE 1 = 1",
        );
        if let Some(before) = before {
            query.push(" AND id < ").push_bind(before);
        }
        if let Some(level) = filter
            .level
            .as_deref()
            .map(str::trim)
            .filter(|l| matches!(*l, "debug" | "info" | "warn" | "error"))
        {
            query.push(" AND level = ").push_bind(level.to_string());
        }
        if let Some(category) = filter
            .category
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            query
                .push(" AND category = ")
                .push_bind(category.to_string());
        }
        if let Some(needle) = filter
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            // 与文件搜索一致用 instr，免得关键字里的 % _ 被当成 LIKE 通配符
            query
                .push(" AND (instr(lower(message), lower(")
                .push_bind(needle.to_string())
                .push(")) > 0 OR instr(lower(category), lower(")
                .push_bind(needle.to_string())
                .push(")) > 0)");
        }
        query
            .push(" ORDER BY id DESC LIMIT ")
            .push_bind(fetch_limit);

        let mut rows = query
            .build_query_as::<BackendLog>()
            .fetch_all(&self.pool)
            .await?;
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        Ok((rows, has_more))
    }

    /// 读取日志切面：类别取值与各级别条数，供管理页筛选下拉与角标使用。
    /// 统计的是全库而非当前页，这样“错误 3 条”即使不在首页也能被看见。
    pub async fn backend_log_facets(&self) -> AppResult<BackendLogFacets> {
        let categories: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT category FROM backend_logs ORDER BY category")
                .fetch_all(&self.pool)
                .await?;
        let counts: Vec<(String, i64)> =
            sqlx::query_as("SELECT level, COUNT(*) FROM backend_logs GROUP BY level")
                .fetch_all(&self.pool)
                .await?;
        Ok(BackendLogFacets {
            categories: categories.into_iter().map(|(c,)| c).collect(),
            level_counts: counts.into_iter().collect(),
        })
    }
}
