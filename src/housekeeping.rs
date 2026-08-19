//! 定期维护任务：回收站清理、断点续传残留清理与多渠道数据库备份。

use crate::db::now_ts;
use crate::error::AppResult;
use crate::settings::Runtime;
use crate::vfs::Vfs;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(30 * 60);
const UPLOAD_SESSION_KEEP_SECS: i64 = 7 * 86_400;

pub async fn run(vfs: Vfs, runtime: Arc<Runtime>, task: crate::tasks::TaskReporter) {
    loop {
        if task.is_cancelled() {
            return;
        }
        task.heartbeat();
        match tick(&vfs, &runtime).await {
            Ok(()) => task.clear_error(),
            Err(error) => {
                task.report_error(&error.to_string());
                tracing::warn!("维护任务出错（下轮重试）：{error}");
            }
        }
        if !task.sleep(TICK).await {
            return;
        }
    }
}

async fn tick(vfs: &Vfs, runtime: &Runtime) -> AppResult<()> {
    vfs.uploader.process_pending_deletes();
    let extras = crate::extras::load(&vfs.db).await?;

    let abandoned = vfs
        .db
        .cleanup_expired_upload_sessions(now_ts() - UPLOAD_SESSION_KEEP_SECS)
        .await?;
    if !abandoned.is_empty() {
        tracing::info!("清理过期断点续传任务：释放 {} 条消息", abandoned.len());
        vfs.uploader.delete_messages_best_effort(abandoned);
    }

    if extras.trash_retention_days > 0 {
        let freed = vfs
            .db
            .trash_purge_expired(extras.trash_retention_days)
            .await?;
        if !freed.is_empty() {
            tracing::info!("回收站超期清理：释放 {} 条消息", freed.len());
            vfs.uploader.delete_messages_best_effort(freed);
        }
    }

    if let Some(outcome) = crate::backup::automatic_if_due(vfs, runtime, &extras).await? {
        tracing::info!(
            file = %outcome.name,
            destinations = outcome.succeeded,
            "数据库自动备份完成"
        );
    }
    Ok(())
}
