//! 分享链接。

use super::*;

impl Db {
    /// 节点的完整路径;自身或任一祖先不是 ready(回收站/上传中)时返回 None
    ///
    /// 一条递归查询取回节点到根的整条祖先链（depth 0 为节点自身，自下而上），
    /// 深目录也只有一次往返；应用媒体与元数据请求每次都经过这里。
    pub async fn full_path_if_visible(&self, id: i64) -> AppResult<Option<String>> {
        let chain: Vec<(i64, Option<i64>, String, String)> = sqlx::query_as(
            "WITH RECURSIVE chain(id, parent_id, name, status, depth) AS (
               SELECT id, parent_id, name, status, 0 FROM nodes WHERE id = ?
               UNION ALL
               SELECT n.id, n.parent_id, n.name, n.status, c.depth + 1
               FROM nodes n JOIN chain c ON n.id = c.parent_id
               WHERE c.depth < 4096
             )
             SELECT id, parent_id, name, status FROM chain ORDER BY depth",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        if chain.is_empty() {
            return Err(AppError::NotFound);
        }
        let mut parts = Vec::with_capacity(chain.len());
        let mut reached_top = false;
        for (node_id, parent_id, name, status) in &chain {
            if *node_id == ROOT_ID {
                reached_top = true;
                break;
            }
            if status != "ready" {
                return Ok(None);
            }
            parts.push(name.as_str());
            if parent_id.is_none() {
                reached_top = true;
                break;
            }
        }
        if !reached_top {
            // 祖先链断裂（父节点缺失或超出深度上限）：与逐级查询遇到缺失父节点时一致
            return Err(AppError::NotFound);
        }
        parts.reverse();
        Ok(Some(format!("/{}", parts.join("/"))))
    }

    pub async fn share_create(
        &self,
        node_id: i64,
        password: Option<&str>,
        expires_at: Option<i64>,
        max_downloads: Option<i64>,
    ) -> AppResult<Share> {
        // 12 位随机 token,碰撞(极小概率)时重试
        for _ in 0..3 {
            let token = random_token(12);
            let r = sqlx::query(
                "INSERT INTO shares (token, node_id, password, expires_at, max_downloads, downloads, created_at)
                 VALUES (?, ?, ?, ?, ?, 0, ?)",
            )
            .bind(&token)
            .bind(node_id)
            .bind(password)
            .bind(expires_at)
            .bind(max_downloads)
            .bind(now_ts())
            .execute(&self.pool)
            .await;
            match r {
                Ok(_) => {
                    return self
                        .share_by_token(&token)
                        .await?
                        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("分享刚创建即丢失")));
                }
                Err(sqlx::Error::Database(e)) if e.is_unique_violation() => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(AppError::Internal(anyhow::anyhow!("生成分享 token 失败")))
    }

    pub async fn share_by_token(&self, token: &str) -> AppResult<Option<Share>> {
        Ok(
            sqlx::query_as::<_, Share>("SELECT * FROM shares WHERE token = ?")
                .bind(token)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn shares_list(&self) -> AppResult<Vec<Share>> {
        Ok(
            sqlx::query_as::<_, Share>("SELECT * FROM shares ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn share_delete(&self, id: i64) -> AppResult<()> {
        sqlx::query("DELETE FROM shares WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 为一张短期票据原子领取某个文件的下载额度。
    ///
    /// 返回 `true` 表示可以下载。相同票据重复读取同一文件（包括 Range 请求）
    /// 不会再次计数；不同票据或目录分享中的不同文件仍会分别领取额度。
    pub async fn share_claim_download(
        &self,
        share_id: i64,
        ticket_id: &str,
        node_id: i64,
        ticket_expires_at: i64,
    ) -> AppResult<bool> {
        let now = now_ts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // 顺手控制短期领取记录的体积；计数已经写入 shares，不随记录清理回退。
        sqlx::query("DELETE FROM share_download_claims WHERE expires_at < ?")
            .bind(now)
            .execute(&mut *tx)
            .await?;

        let inserted = sqlx::query(
            "INSERT INTO share_download_claims
                (share_id, ticket_id, node_id, expires_at, claimed_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(share_id, ticket_id, node_id) DO NOTHING",
        )
        .bind(share_id)
        .bind(ticket_id)
        .bind(node_id)
        .bind(ticket_expires_at)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        if !inserted {
            tx.commit().await?;
            return Ok(true);
        }

        let claimed = sqlx::query(
            "UPDATE shares
             SET downloads = downloads + 1
             WHERE id = ?
               AND (max_downloads IS NULL OR downloads < max_downloads)",
        )
        .bind(share_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        if claimed {
            tx.commit().await?;
        } else {
            // 领取记录与计数必须同成同败，不能留下一个未计数却可复用的票据。
            tx.rollback().await?;
        }
        Ok(claimed)
    }
}
