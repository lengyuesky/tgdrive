//! 收件请求、恢复及直接入库事务。待确认请求不使用 uploading 网盘占位。

use super::*;
use serde::Serialize;
use sqlx::SqliteConnection;

pub const INBOX_TTL: i64 = 24 * 60 * 60;
pub const INBOX_HISTORY_TTL: i64 = 30 * 24 * 60 * 60;
pub const INBOX_USER_LIMIT: i64 = 50;
pub const INBOX_TOTAL_LIMIT: i64 = 500;

/// 只操作恢复候选或导出的快照，不能对在线数据库调用。
pub(crate) async fn invalidate_restored_inbox(conn: &mut SqliteConnection) -> AppResult<()> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='inbox_requests'",
    )
    .fetch_one(&mut *conn)
    .await?;
    if exists > 0 {
        sqlx::query("UPDATE inbox_requests SET status='invalidated',ended_at=?,updated_at=?,rename_prompt_id=NULL,last_error='数据库恢复后旧收件已失效，请重新发送文件',notice_pending=1,notice_after=0,revision=revision+1 WHERE ended_at IS NULL")
            .bind(now_ts()).bind(now_ts()).execute(&mut *conn).await?;
    }
    Ok(())
}

#[derive(Clone, sqlx::FromRow)]
pub struct InboxRequest {
    pub id: String,
    pub bot_id: String,
    pub origin_target_id: Option<i64>,
    pub chat_id: i64,
    pub source_message_id: i64,
    pub media_kind: String,
    pub original_name: String,
    pub original_suffix: String,
    pub name: String,
    pub file_id: String,
    pub file_size: i64,
    pub thumbnail_file_id: Option<String>,
    pub status: String,
    pub selected_dir_id: Option<i64>,
    pub selected_path: Option<String>,
    pub operation_message_id: Option<i64>,
    pub rename_prompt_id: Option<i64>,
    pub storage_target_id: Option<i64>,
    pub copied_message_id: Option<i64>,
    pub copied_file_id: Option<String>,
    pub copied_size: Option<i64>,
    pub copied_thumbnail_file_id: Option<String>,
    pub revision: i64,
    pub config_revision: i64,
    pub copy_started_at: Option<i64>,
    pub node_id: Option<i64>,
    pub result_path: Option<String>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
    pub ended_at: Option<i64>,
    pub notice_attempts: i64,
}

impl InboxRequest {
    pub fn editable(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "renaming" | "failed") && self.ended_at.is_none()
    }
    pub fn cancellable(&self) -> bool {
        matches!(
            self.status.as_str(),
            "pending" | "renaming" | "queued" | "failed" | "uncertain"
        ) && self.ended_at.is_none()
    }
    pub fn view(&self) -> InboxRequestView {
        InboxRequestView {
            id: self.id.clone(),
            bot_id: self.bot_id.clone(),
            chat_id: self.chat_id,
            source_message_id: self.source_message_id,
            original_name: self.original_name.clone(),
            name: self.name.clone(),
            status: self.status.clone(),
            selected_path: self.selected_path.clone(),
            node_id: self.node_id,
            result_path: self.result_path.clone(),
            last_error: self.last_error.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            expires_at: self.expires_at,
            ended_at: self.ended_at,
            can_cancel: self.cancellable(),
            can_retry: self.status == "failed" && self.expires_at > now_ts(),
            target_channel: None,
            stored_message_id: self.copied_message_id,
            copy_started_at: self.copy_started_at,
        }
    }
}

/// 管理接口只返回处理摘要，不暴露 Telegram file_id、缩略图引用或存储凭据。
#[derive(Serialize)]
pub struct InboxRequestView {
    pub id: String,
    pub bot_id: String,
    pub chat_id: i64,
    pub source_message_id: i64,
    pub original_name: String,
    pub name: String,
    pub status: String,
    pub selected_path: Option<String>,
    pub node_id: Option<i64>,
    pub result_path: Option<String>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
    pub ended_at: Option<i64>,
    pub can_cancel: bool,
    pub can_retry: bool,
    pub target_channel: Option<String>,
    pub stored_message_id: Option<i64>,
    pub copy_started_at: Option<i64>,
}

pub struct NewInboxRequest<'a> {
    pub origin_target_id: Option<i64>,
    pub bot_id: &'a str,
    pub chat_id: i64,
    pub message_id: i64,
    pub media_kind: &'a str,
    pub name: &'a str,
    pub suffix: &'a str,
    pub file_id: &'a str,
    pub size: i64,
    pub thumbnail: Option<&'a str>,
    pub selected_dir_id: Option<i64>,
    pub selected_path: Option<&'a str>,
}

pub struct InboxFile<'a> {
    pub name: &'a str,
    pub suffix: &'a str,
    pub size: i64,
    pub file_id: &'a str,
    pub thumbnail: Option<&'a str>,
    pub message_id: i64,
    pub target_id: i64,
    pub source: InboxSource,
}

async fn directory_path(conn: &mut SqliteConnection, id: i64) -> AppResult<String> {
    let rows: Vec<(i64, Option<i64>, String, bool, String, i64)> = sqlx::query_as(
        "WITH RECURSIVE chain(id,parent_id,name,is_dir,status,depth) AS (
             SELECT id,parent_id,name,is_dir,status,0 FROM nodes WHERE id=?
             UNION ALL SELECT n.id,n.parent_id,n.name,n.is_dir,n.status,c.depth+1
             FROM nodes n JOIN chain c ON n.id=c.parent_id WHERE c.depth<4096)
         SELECT id,parent_id,name,is_dir,status,depth FROM chain ORDER BY depth",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty()
        || rows
            .last()
            .is_none_or(|row| row.0 != ROOT_ID || row.1.is_some())
        || rows.iter().any(|row| row.4 != "ready")
    {
        return Err(AppError::NotFound);
    }
    if rows.iter().any(|row| !row.3) {
        return Err(AppError::NotDirectory);
    }
    Ok(format!(
        "/{}",
        rows.into_iter()
            .rev()
            .filter(|row| row.0 != ROOT_ID)
            .map(|row| row.2)
            .collect::<Vec<_>>()
            .join("/")
    ))
}

pub(crate) fn numbered_name(name: &str, original_suffix: &str, number: u32) -> AppResult<String> {
    if number == 0 {
        return Ok(name.into());
    }
    let dot = if !original_suffix.is_empty() && name.ends_with(original_suffix) {
        name.len() - original_suffix.len()
    } else {
        name.rfind('.')
            .filter(|index| *index > 0)
            .unwrap_or(name.len())
    };
    let suffix = format!(" ({number}){}", &name[dot..]);
    if suffix.len() >= 255 {
        return Err(AppError::BadRequest(
            "文件后缀过长，无法生成不重名的名称，请先重命名".into(),
        ));
    }
    let mut stem = name[..dot].to_string();
    while stem.len() + suffix.len() > 255 {
        stem.pop();
    }
    Ok(format!("{stem}{suffix}"))
}

async fn insert_file(
    conn: &mut SqliteConnection,
    dir_id: i64,
    file: &InboxFile<'_>,
) -> AppResult<(i64, String)> {
    let path = directory_path(conn, dir_id).await?;
    if dir_id == ROOT_ID
        || file.size <= 0
        || file.name.is_empty()
        || file.name.len() > 255
        || crate::vfs::normalize(file.name)?.as_slice() != [file.name]
    {
        return Err(AppError::BadRequest(
            "收件目录、文件名或文件大小无效".into(),
        ));
    }
    for number in 0..=999 {
        let name = numbered_name(file.name, file.suffix, number)?;
        let occupied: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM nodes WHERE parent_id=? AND name=? AND status IN ('ready','uploading')")
            .bind(dir_id).bind(&name).fetch_one(&mut *conn).await?;
        if occupied.0 > 0 {
            continue;
        }
        let timestamp = now_ts();
        let id = sqlx::query("INSERT INTO nodes(parent_id,name,is_dir,size,status,created_at,modified_at) VALUES(?,?,0,?,'ready',?,?)")
            .bind(dir_id).bind(&name).bind(file.size).bind(timestamp).bind(timestamp).execute(&mut *conn).await?.last_insert_rowid();
        sqlx::query("INSERT INTO chunks(node_id,idx,size,tg_file_id,tg_message_id,storage_target_id,tg_thumbnail_file_id) VALUES(?,0,?,?,?,?,?)")
            .bind(id).bind(file.size).bind(file.file_id).bind(file.message_id).bind(file.target_id).bind(file.thumbnail).execute(&mut *conn).await?;
        sqlx::query("INSERT INTO inbox_sources(node_id,source) VALUES(?,?)")
            .bind(id)
            .bind(file.source.as_str())
            .execute(&mut *conn)
            .await?;
        return Ok((id, format!("{}/{}", path.trim_end_matches('/'), name)));
    }
    Err(AppError::AlreadyExists)
}

async fn enqueue_copy_cleanup(
    conn: &mut SqliteConnection,
    request: &InboxRequest,
) -> AppResult<()> {
    if let (Some(target), Some(message)) = (request.storage_target_id, request.copied_message_id) {
        sqlx::query("INSERT INTO delete_jobs(storage_target_id,tg_message_id,attempts,next_retry_at,created_at) VALUES(?,?,0,?,?) ON CONFLICT(storage_target_id,tg_message_id) DO NOTHING")
            .bind(target).bind(message).bind(now_ts() + DELETE_GRACE_SECS).bind(now_ts()).execute(&mut *conn).await?;
    }
    Ok(())
}

impl Db {
    pub async fn inbox_known_storage_bot(&self, sender_id: i64) -> AppResult<bool> {
        let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM storage_targets WHERE substr(bot_token,1,instr(bot_token,':')-1)=?)")
            .bind(sender_id.to_string()).fetch_one(&self.pool).await?;
        Ok(exists != 0)
    }

    /// 一次读取全部配置目录的当前路径与可见性，避免每条收件逐目录查询。
    pub async fn inbox_directory_paths(&self, ids: &[i64]) -> AppResult<HashMap<i64, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let unique: HashSet<_> = ids.iter().copied().filter(|id| *id > ROOT_ID).collect();
        let ids = serde_json::to_string(&unique).map_err(anyhow::Error::from)?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "WITH RECURSIVE chain(origin,id,parent_id,path,visible,depth) AS (
               SELECT n.id,n.id,n.parent_id,'/'||n.name,n.is_dir=1 AND n.status='ready',0
               FROM nodes n JOIN json_each(?) wanted ON n.id=wanted.value
               UNION ALL
               SELECT c.origin,n.id,n.parent_id,CASE WHEN n.id=1 THEN c.path ELSE '/'||n.name||c.path END,
                      c.visible AND n.is_dir=1 AND n.status='ready',c.depth+1
               FROM nodes n JOIN chain c ON n.id=c.parent_id
               WHERE c.depth<4096 AND length(CAST(c.path AS BLOB))<=2048)
             SELECT origin,path FROM chain WHERE id=1 AND parent_id IS NULL AND visible=1 AND length(CAST(path AS BLOB))<=2048",
        ).bind(ids).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }
    pub async fn inbox_offset(&self, bot_id: &str) -> AppResult<i64> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let key = format!("extra.inbox_offset.{bot_id}");
        let stored: Option<(String,)> = sqlx::query_as("SELECT v FROM settings WHERE k=?")
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(stored) = stored {
            return Ok(stored.0.parse().unwrap_or(0));
        }
        let migrated: Option<(String,)> =
            sqlx::query_as("SELECT v FROM settings WHERE k='extra.inbox_offset_owner'")
                .fetch_optional(&mut *tx)
                .await?;
        let offset = if migrated.is_none() {
            let legacy: Option<(String,)> =
                sqlx::query_as("SELECT v FROM settings WHERE k='extra.inbox_offset'")
                    .fetch_optional(&mut *tx)
                    .await?;
            sqlx::query("INSERT INTO settings(k,v) VALUES('extra.inbox_offset_owner',?)")
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
            legacy
                .and_then(|value| value.0.parse::<i64>().ok())
                .unwrap_or(0)
        } else {
            0
        };
        sqlx::query("INSERT INTO settings(k,v) VALUES(?,?)")
            .bind(&key)
            .bind(offset.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(offset)
    }

    pub async fn set_inbox_offset(&self, bot_id: &str, offset: i64) -> AppResult<()> {
        self.settings_apply(
            &[
                (format!("extra.inbox_offset.{bot_id}"), offset.to_string()),
                ("extra.inbox_offset".into(), offset.to_string()),
            ],
            &[],
        )
        .await
    }

    pub async fn inbox_directory_path(&self, id: i64) -> AppResult<String> {
        let mut connection = self.pool.acquire().await?;
        directory_path(&mut connection, id).await
    }

    /// 校验目录版本与设置写入共用事务，拒绝并发覆盖时不部分保存其他字段。
    pub async fn apply_inbox_settings(
        &self,
        sets: &[(String, String)],
        deletes: &[String],
        expected_revision: Option<i64>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(expected) = expected_revision {
            let actual: Option<(i64,)> = sqlx::query_as(
                "SELECT CAST(v AS INTEGER) FROM settings WHERE k='extra.inbox_routes_revision'",
            )
            .fetch_optional(&mut *tx)
            .await?;
            if actual.map(|value| value.0).unwrap_or(0) != expected {
                return Err(AppError::Conflict(
                    "收件目录配置已被更新，请重新加载后再保存".into(),
                ));
            }
        }
        let mut changed = false;
        let mut presentation_changed = false;
        // 旧合并开关只迁移缺失的字段；现代客户端单改一个开关不会覆盖另一个。
        if deletes.iter().any(|key| key == "extra.inbox_enabled") {
            sqlx::query("INSERT INTO settings(k,v) SELECT 'extra.inbox_bot_enabled',v FROM settings WHERE k='extra.inbox_enabled' ON CONFLICT(k) DO NOTHING").execute(&mut *tx).await?;
            sqlx::query("INSERT INTO settings(k,v) SELECT 'extra.inbox_channel_enabled',v FROM settings WHERE k='extra.inbox_enabled' ON CONFLICT(k) DO NOTHING").execute(&mut *tx).await?;
        }
        for (key, value) in sets {
            let previous: Option<(String,)> = sqlx::query_as("SELECT v FROM settings WHERE k=?")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?;
            if key.starts_with("extra.inbox_")
                && previous.as_ref().map(|value| &value.0) != Some(value)
            {
                presentation_changed = true;
                if key.ends_with("_dir") || key.ends_with("_rules") || key.ends_with("_routes") {
                    changed = true;
                }
            }
            sqlx::query(
                "INSERT INTO settings(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        for key in deletes {
            sqlx::query("DELETE FROM settings WHERE k=?")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        if changed {
            sqlx::query("INSERT INTO settings(k,v) VALUES('extra.inbox_routes_revision','1') ON CONFLICT(k) DO UPDATE SET v=CAST(v AS INTEGER)+1").execute(&mut *tx).await?;
        }
        if presentation_changed {
            sqlx::query("UPDATE inbox_requests SET revision=revision+1,notice_pending=1,notice_after=0 WHERE ended_at IS NULL").execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn install_inbox_routes(&self, revision: i64, raw: &str) -> AppResult<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actual: Option<(i64,)> = sqlx::query_as(
            "SELECT CAST(v AS INTEGER) FROM settings WHERE k='extra.inbox_routes_revision'",
        )
        .fetch_optional(&mut *tx)
        .await?;
        if actual.map(|v| v.0).unwrap_or(0) != revision {
            return Ok(false);
        }
        sqlx::query("INSERT INTO settings(k,v) VALUES('extra.inbox_routes',?) ON CONFLICT(k) DO UPDATE SET v=excluded.v").bind(raw).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO settings(k,v) VALUES('extra.inbox_routes_revision','1') ON CONFLICT(k) DO UPDATE SET v=CAST(v AS INTEGER)+1").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn inbox_request(&self, id: &str) -> AppResult<InboxRequest> {
        sqlx::query_as("SELECT * FROM inbox_requests WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    #[cfg(test)]
    pub async fn inbox_source_request(
        &self,
        bot_id: &str,
        chat_id: i64,
        message_id: i64,
    ) -> AppResult<Option<InboxRequest>> {
        Ok(sqlx::query_as(
            "SELECT * FROM inbox_requests WHERE bot_id=? AND chat_id=? AND source_message_id=?",
        )
        .bind(bot_id)
        .bind(chat_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn inbox_has_requests(&self, bot_id: &str) -> AppResult<bool> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM inbox_requests WHERE bot_id=? LIMIT 1")
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    pub async fn create_inbox_request(
        &self,
        input: NewInboxRequest<'_>,
    ) -> AppResult<InboxRequest> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let old: Option<InboxRequest> = sqlx::query_as(
            "SELECT * FROM inbox_requests WHERE bot_id=? AND chat_id=? AND source_message_id=?",
        )
        .bind(input.bot_id)
        .bind(input.chat_id)
        .bind(input.message_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(old) = old {
            return Ok(old);
        }
        let (total, user): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*),COALESCE(SUM(chat_id=?),0) FROM inbox_requests WHERE ended_at IS NULL",
        )
        .bind(input.chat_id)
        .fetch_one(&mut *tx)
        .await?;
        if total >= INBOX_TOTAL_LIMIT || user >= INBOX_USER_LIMIT {
            return Err(AppError::Conflict(
                "收件待办已达上限，请先处理或取消已有文件后重新发送".into(),
            ));
        }
        let id = format!("{:032x}", rand::random::<u128>());
        let timestamp = now_ts();
        sqlx::query("INSERT INTO inbox_requests(id,bot_id,origin_target_id,chat_id,source_message_id,media_kind,original_name,original_suffix,name,file_id,file_size,thumbnail_file_id,selected_dir_id,selected_path,created_at,updated_at,expires_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&id).bind(input.bot_id).bind(input.origin_target_id).bind(input.chat_id).bind(input.message_id).bind(input.media_kind).bind(input.name).bind(input.suffix).bind(input.name)
            .bind(input.file_id).bind(input.size).bind(input.thumbnail).bind(input.selected_dir_id).bind(input.selected_path)
            .bind(timestamp).bind(timestamp).bind(timestamp + INBOX_TTL).execute(&mut *tx).await?;
        tx.commit().await?;
        self.inbox_request(&id).await
    }

    pub async fn inbox_set_selection(
        &self,
        id: &str,
        directory: Option<i64>,
        path: Option<&str>,
    ) -> AppResult<bool> {
        Ok(sqlx::query("UPDATE inbox_requests SET revision=revision+1,selected_dir_id=?,selected_path=?,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND status IN ('pending','renaming','failed') AND expires_at>? AND ended_at IS NULL")
            .bind(directory).bind(path).bind(now_ts()).bind(id).bind(now_ts()).execute(&self.pool).await?.rows_affected() > 0)
    }

    pub async fn inbox_set_rename_prompt(&self, id: &str, message_id: i64) -> AppResult<bool> {
        Ok(sqlx::query("UPDATE inbox_requests SET revision=revision+1,status='renaming',rename_prompt_id=?,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND status IN ('pending','renaming','failed') AND expires_at>? AND ended_at IS NULL")
            .bind(message_id).bind(now_ts()).bind(id).bind(now_ts()).execute(&self.pool).await?.rows_affected() > 0)
    }

    pub async fn inbox_rename_request(
        &self,
        bot_id: &str,
        chat_id: i64,
        prompt_id: i64,
    ) -> AppResult<Option<InboxRequest>> {
        Ok(sqlx::query_as("SELECT * FROM inbox_requests WHERE bot_id=? AND chat_id=? AND rename_prompt_id=? AND status='renaming' AND ended_at IS NULL AND expires_at>?")
            .bind(bot_id).bind(chat_id).bind(prompt_id).bind(now_ts()).fetch_optional(&self.pool).await?)
    }

    pub async fn inbox_set_name(&self, id: &str, prompt_id: i64, name: &str) -> AppResult<bool> {
        Ok(sqlx::query("UPDATE inbox_requests SET revision=revision+1,name=?,status='pending',rename_prompt_id=NULL,last_error=NULL,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND rename_prompt_id=? AND status='renaming' AND expires_at>? AND ended_at IS NULL")
            .bind(name).bind(now_ts()).bind(id).bind(prompt_id).bind(now_ts()).execute(&self.pool).await?.rows_affected() > 0)
    }

    pub async fn inbox_abort_rename(&self, id: &str) -> AppResult<bool> {
        Ok(sqlx::query("UPDATE inbox_requests SET revision=revision+1,status=CASE WHEN last_error IS NULL THEN 'pending' ELSE 'failed' END,rename_prompt_id=NULL,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND status='renaming' AND expires_at>? AND ended_at IS NULL")
            .bind(now_ts()).bind(id).bind(now_ts()).execute(&self.pool).await?.rows_affected() > 0)
    }

    pub async fn inbox_queue_request(
        &self,
        id: &str,
        request_revision: i64,
        config_revision: i64,
    ) -> AppResult<bool> {
        Ok(sqlx::query("UPDATE inbox_requests SET revision=revision+1,status='queued',config_revision=?,rename_prompt_id=NULL,last_error=NULL,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND revision=? AND status IN ('pending','failed') AND selected_dir_id IS NOT NULL AND expires_at>? AND ended_at IS NULL")
            .bind(config_revision).bind(now_ts()).bind(id).bind(request_revision).bind(now_ts()).execute(&self.pool).await?.rows_affected() > 0)
    }

    pub async fn inbox_cancel_request(&self, id: &str) -> AppResult<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let request: Option<InboxRequest> =
            sqlx::query_as("SELECT * FROM inbox_requests WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(request) = request else {
            return Err(AppError::NotFound);
        };
        if !request.cancellable() {
            return Ok(false);
        }
        enqueue_copy_cleanup(&mut tx, &request).await?;
        sqlx::query("UPDATE inbox_requests SET revision=revision+1,status='cancelled',ended_at=?,updated_at=?,rename_prompt_id=NULL,notice_pending=1,notice_after=0 WHERE id=?")
            .bind(now_ts()).bind(now_ts()).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn inbox_resend_notice(&self, id: &str) -> AppResult<()> {
        sqlx::query("UPDATE inbox_requests SET revision=revision+1,operation_message_id=NULL,rename_prompt_id=NULL,status=CASE WHEN status='renaming' THEN CASE WHEN last_error IS NULL THEN 'pending' ELSE 'failed' END ELSE status END,notice_pending=1,notice_after=0,notice_attempts=0 WHERE id=?")
            .bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn inbox_mark_notice(&self, id: &str) -> AppResult<()> {
        sqlx::query("UPDATE inbox_requests SET revision=revision+1,notice_pending=1,notice_after=0 WHERE id=?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn inbox_next_work(&self, bot_id: &str) -> AppResult<Option<InboxRequest>> {
        Ok(sqlx::query_as("SELECT * FROM inbox_requests WHERE status='copied' OR (bot_id=? AND status='queued' AND expires_at>?) ORDER BY CASE WHEN status='copied' THEN 0 ELSE 1 END,updated_at,id LIMIT 1")
            .bind(bot_id).bind(now_ts()).fetch_optional(&self.pool).await?)
    }

    pub async fn inbox_claim_request(
        &self,
        id: &str,
        target_id: i64,
        revision: i64,
    ) -> AppResult<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let values: Vec<(String, String)> = sqlx::query_as("SELECT k,v FROM settings WHERE k IN ('extra.inbox_routes_revision','extra.inbox_bot_enabled','extra.inbox_enabled','extra.inbox_allowed_chats')").fetch_all(&mut *tx).await?;
        let values: HashMap<_, _> = values.into_iter().collect();
        let actual = values
            .get("extra.inbox_routes_revision")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let enabled = values
            .get("extra.inbox_bot_enabled")
            .or_else(|| values.get("extra.inbox_enabled"))
            .is_some_and(|value| value == "1");
        let chat: Option<(i64,)> = sqlx::query_as("SELECT chat_id FROM inbox_requests WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let authorized = chat.is_some_and(|chat| {
            values
                .get("extra.inbox_allowed_chats")
                .is_some_and(|value| {
                    value
                        .split(',')
                        .any(|part| part.trim().parse::<i64>().ok() == Some(chat.0))
                })
        });
        if actual != revision || !enabled || !authorized {
            return Ok(false);
        }
        let changed = sqlx::query("UPDATE inbox_requests SET revision=revision+1,status=CASE WHEN copied_message_id IS NULL THEN 'copying' ELSE 'copied' END,storage_target_id=CASE WHEN copied_message_id IS NULL THEN ? ELSE storage_target_id END,config_revision=?,copy_started_at=CASE WHEN copied_message_id IS NULL THEN ? ELSE copy_started_at END,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND status='queued' AND expires_at>? AND ended_at IS NULL")
            .bind(target_id).bind(revision).bind(now_ts()).bind(now_ts()).bind(id).bind(now_ts()).execute(&mut *tx).await?.rows_affected() > 0;
        tx.commit().await?;
        Ok(changed)
    }

    /// 仅供尚未调用 Telegram 的工作在配置切换后归还待办。
    pub async fn inbox_unclaim_unsent(&self, id: &str) -> AppResult<()> {
        sqlx::query("UPDATE inbox_requests SET revision=revision+1,status='queued',storage_target_id=NULL,copy_started_at=NULL,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND status='copying' AND copied_message_id IS NULL AND ended_at IS NULL")
            .bind(now_ts()).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn inbox_record_copy(
        &self,
        id: &str,
        message: i64,
        file_id: &str,
        size: i64,
        thumbnail: Option<&str>,
    ) -> AppResult<()> {
        let changed = sqlx::query("UPDATE inbox_requests SET revision=revision+1,status='copied',copied_message_id=?,copied_file_id=?,copied_size=?,copied_thumbnail_file_id=?,updated_at=? WHERE id=? AND status='copying' AND ended_at IS NULL")
            .bind(message).bind(file_id).bind(size).bind(thumbnail).bind(now_ts()).bind(id).execute(&self.pool).await?.rows_affected();
        if changed == 0 {
            return Err(AppError::Conflict("收件状态已变化".into()));
        }
        Ok(())
    }

    pub async fn inbox_fail_request(
        &self,
        id: &str,
        error: &str,
        uncertain: bool,
    ) -> AppResult<()> {
        sqlx::query("UPDATE inbox_requests SET revision=revision+1,status=?,last_error=?,updated_at=?,notice_pending=1,notice_after=0 WHERE id=? AND status IN ('queued','copying','copied') AND ended_at IS NULL")
            .bind(if uncertain { "uncertain" } else { "failed" }).bind(error.chars().take(500).collect::<String>()).bind(now_ts()).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn recover_inbox_requests(&self) -> AppResult<()> {
        sqlx::query("UPDATE inbox_requests SET revision=revision+1,status=CASE WHEN copied_message_id IS NULL THEN 'uncertain' ELSE 'copied' END,last_error=CASE WHEN copied_message_id IS NULL THEN '服务重启，无法确认上次转存结果，请人工核实频道' ELSE last_error END,notice_pending=1,notice_after=0,updated_at=? WHERE status='copying' AND ended_at IS NULL")
            .bind(now_ts()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn maintain_inbox_requests(&self, bot_id: &str, allowed: &[i64]) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let requests: Vec<InboxRequest> = sqlx::query_as("SELECT * FROM inbox_requests WHERE status IN ('pending','renaming','queued','failed') AND ended_at IS NULL").fetch_all(&mut *tx).await?;
        for request in requests {
            let reason = if !bot_id.is_empty() && request.bot_id != bot_id {
                Some(("invalidated", "收件 Bot 已更换，请向新 Bot 重新发送文件"))
            } else if !allowed.contains(&request.chat_id) {
                Some(("invalidated", "用户已移出收件白名单"))
            } else if request.expires_at <= now_ts() {
                Some(("expired", "收件已超过 24 小时，请重新发送文件"))
            } else {
                None
            };
            if let Some((status, reason)) = reason {
                enqueue_copy_cleanup(&mut tx, &request).await?;
                sqlx::query("UPDATE inbox_requests SET revision=revision+1,status=?,last_error=?,ended_at=?,updated_at=?,rename_prompt_id=NULL,notice_pending=1,notice_after=0 WHERE id=?")
                    .bind(status).bind(reason).bind(now_ts()).bind(now_ts()).bind(&request.id).execute(&mut *tx).await?;
            }
        }
        sqlx::query("DELETE FROM inbox_requests WHERE ended_at IS NOT NULL AND ended_at<?")
            .bind(now_ts() - INBOX_HISTORY_TTL)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn inbox_notices(&self, bot_id: &str) -> AppResult<Vec<InboxRequest>> {
        Ok(sqlx::query_as("SELECT * FROM inbox_requests WHERE bot_id=? AND notice_pending=1 AND notice_after<=? ORDER BY updated_at,id LIMIT 20")
            .bind(bot_id).bind(now_ts()).fetch_all(&self.pool).await?)
    }

    pub async fn inbox_retired_notices(&self, bot_id: &str) -> AppResult<Vec<InboxRequest>> {
        // 只向旧请求发送一次有限重试的结束通知，不再监听或处理旧 Bot 的输入。
        Ok(sqlx::query_as("SELECT * FROM inbox_requests WHERE bot_id!=? AND origin_target_id IS NOT NULL AND notice_pending=1 AND notice_after<=? AND notice_attempts<3 AND (ended_at IS NOT NULL OR status='uncertain') ORDER BY updated_at,id LIMIT 1")
            .bind(bot_id).bind(now_ts()).fetch_all(&self.pool).await?)
    }

    pub async fn inbox_notice_claim(&self, id: &str, revision: i64) -> AppResult<bool> {
        Ok(sqlx::query("UPDATE inbox_requests SET notice_after=? WHERE id=? AND revision=? AND notice_pending=1 AND notice_after<=?")
            .bind(now_ts() + 60).bind(id).bind(revision).bind(now_ts()).execute(&self.pool).await?.rows_affected() > 0)
    }

    pub async fn inbox_notice_done(
        &self,
        id: &str,
        message_id: i64,
        observed_update: i64,
        observed_status: &str,
    ) -> AppResult<()> {
        // 网络往返期间状态可能变化；保留通知标记，下一轮再更新最新状态。
        sqlx::query("UPDATE inbox_requests SET operation_message_id=CASE WHEN revision=? OR operation_message_id IS NULL THEN ? ELSE operation_message_id END,notice_pending=CASE WHEN revision=? AND status=? THEN 0 ELSE 1 END,notice_after=0,notice_attempts=0 WHERE id=?")
            .bind(observed_update).bind(message_id).bind(observed_update).bind(observed_status).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn inbox_notice_failed(&self, id: &str, retry_after: i64) -> AppResult<()> {
        sqlx::query(
            "UPDATE inbox_requests SET notice_after=?,notice_attempts=notice_attempts+1 WHERE id=?",
        )
        .bind(now_ts() + retry_after)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn inbox_requests_page(
        &self,
        active: bool,
        before: Option<(i64, String)>,
        limit: i64,
    ) -> AppResult<Vec<InboxRequest>> {
        let mut query =
            sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT * FROM inbox_requests WHERE ");
        query.push(if active {
            "ended_at IS NULL"
        } else {
            "ended_at IS NOT NULL"
        });
        if let Some((timestamp, id)) = before {
            query
                .push(" AND (created_at,id)<(")
                .push_bind(timestamp)
                .push(",")
                .push_bind(id)
                .push(")");
        }
        query
            .push(" ORDER BY created_at DESC,id DESC LIMIT ")
            .push_bind(limit.clamp(1, 101));
        Ok(query.build_query_as().fetch_all(&self.pool).await?)
    }

    pub async fn register_inbox_file(
        &self,
        directory_id: i64,
        file: InboxFile<'_>,
    ) -> AppResult<String> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (_, path) = insert_file(&mut tx, directory_id, &file).await?;
        tx.commit().await?;
        Ok(path)
    }

    pub async fn finish_inbox_request(&self, id: &str) -> AppResult<String> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let request: InboxRequest = sqlx::query_as("SELECT * FROM inbox_requests WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        if request.status == "stored" {
            return request.result_path.ok_or(AppError::NotFound);
        }
        if request.status != "copied" || request.ended_at.is_some() {
            return Err(AppError::Conflict("收件尚未确认转存结果".into()));
        }
        let revision: Option<(i64,)> = sqlx::query_as(
            "SELECT CAST(v AS INTEGER) FROM settings WHERE k='extra.inbox_routes_revision'",
        )
        .fetch_optional(&mut *tx)
        .await?;
        if revision.map(|v| v.0).unwrap_or(0) != request.config_revision {
            return Err(AppError::Conflict(
                "收件目录配置已变化，请重新确认目录".into(),
            ));
        }
        let dir_id = request
            .selected_dir_id
            .ok_or_else(|| AppError::BadRequest("尚未选择收件目录".into()))?;
        let path = directory_path(&mut tx, dir_id).await?;
        if request.selected_path.as_deref() != Some(path.as_str()) {
            return Err(AppError::Conflict(
                "目录已改名或移动，请重新确认新路径".into(),
            ));
        }
        let file = InboxFile {
            name: &request.name,
            suffix: &request.original_suffix,
            size: request.copied_size.unwrap_or(request.file_size),
            file_id: request
                .copied_file_id
                .as_deref()
                .ok_or(AppError::NotFound)?,
            thumbnail: request.copied_thumbnail_file_id.as_deref(),
            message_id: request.copied_message_id.ok_or(AppError::NotFound)?,
            target_id: request.storage_target_id.ok_or(AppError::NotFound)?,
            source: InboxSource::Bot,
        };
        let (node_id, result) = insert_file(&mut tx, dir_id, &file).await?;
        sqlx::query("UPDATE inbox_requests SET revision=revision+1,status='stored',name=?,node_id=?,result_path=?,ended_at=?,updated_at=?,last_error=NULL,notice_pending=1,notice_after=0 WHERE id=?")
            .bind(result.rsplit('/').next().unwrap_or(&request.name)).bind(node_id).bind(&result).bind(now_ts()).bind(now_ts()).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result)
    }
}
