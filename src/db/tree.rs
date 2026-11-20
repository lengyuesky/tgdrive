//! 目录创建、移动与重命名；连接级原语供网盘接口和应用元数据事务共用。
use super::*;
use sqlx::SqliteConnection;

impl Db {
    pub(crate) async fn node_in(connection: &mut SqliteConnection, id: i64) -> AppResult<Node> {
        sqlx::query_as("SELECT * FROM nodes WHERE id=?")
            .bind(id)
            .fetch_optional(connection)
            .await?
            .ok_or(AppError::NotFound)
    }
    pub(crate) async fn mkdir_in(
        connection: &mut SqliteConnection,
        parent_id: i64,
        name: &str,
    ) -> AppResult<Node> {
        let parent = Self::node_in(connection, parent_id).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id=? AND name=? AND status='ready')",
        )
        .bind(parent_id)
        .bind(name)
        .fetch_one(&mut *connection)
        .await?;
        if exists {
            return Err(AppError::AlreadyExists);
        }
        let ts = now_ts();
        let id = sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) VALUES(?,?,1,0,'ready',?,?)")
            .bind(parent_id).bind(name).bind(ts).bind(ts).execute(&mut *connection).await?.last_insert_rowid();
        Self::node_in(connection, id).await
    }
    pub async fn mkdir(&self, parent_id: i64, name: &str) -> AppResult<Node> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let node = Self::mkdir_in(&mut tx, parent_id, name).await?;
        tx.commit().await?;
        Ok(node)
    }
    pub(crate) async fn is_ancestor_in(
        connection: &mut SqliteConnection,
        ancestor: i64,
        node: i64,
    ) -> AppResult<bool> {
        let row: Option<(i64,)> = sqlx::query_as("WITH RECURSIVE chain(id,depth) AS (SELECT ?,0 UNION ALL SELECT n.parent_id,c.depth+1 FROM nodes n JOIN chain c ON n.id=c.id WHERE n.parent_id IS NOT NULL AND c.depth<4096) SELECT 1 FROM chain WHERE id=? LIMIT 1")
            .bind(node).bind(ancestor).fetch_optional(connection).await?;
        Ok(row.is_some())
    }
    pub async fn is_ancestor(&self, ancestor: i64, node: i64) -> AppResult<bool> {
        let mut connection = self.pool.acquire().await?;
        Self::is_ancestor_in(&mut connection, ancestor, node).await
    }
    pub(crate) async fn move_in(
        connection: &mut SqliteConnection,
        id: i64,
        new_parent: i64,
        new_name: &str,
    ) -> AppResult<()> {
        let node = Self::node_in(connection, id).await?;
        if id == ROOT_ID {
            return Err(AppError::BadRequest("不能移动根目录".into()));
        }
        let parent = Self::node_in(connection, new_parent).await?;
        if !parent.is_dir {
            return Err(AppError::NotDirectory);
        }
        if node.is_dir && Self::is_ancestor_in(connection, id, new_parent).await? {
            return Err(AppError::BadRequest("不能把目录移动到自己内部".into()));
        }
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE parent_id=? AND name=? AND status='ready' AND id<>?)").bind(new_parent).bind(new_name).bind(id).fetch_one(&mut *connection).await?;
        if exists {
            return Err(AppError::AlreadyExists);
        }
        sqlx::query("UPDATE nodes SET parent_id=?,name=?,modified_at=? WHERE id=?")
            .bind(new_parent)
            .bind(new_name)
            .bind(now_ts())
            .bind(id)
            .execute(connection)
            .await?;
        Ok(())
    }
    pub async fn move_node(&self, id: i64, new_parent: i64, new_name: &str) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        Self::move_in(&mut tx, id, new_parent, new_name).await?;
        tx.commit().await?;
        Ok(())
    }
}
