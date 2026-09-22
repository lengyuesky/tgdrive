//! 定期维护任务：回收站清理、断点续传残留清理与多渠道数据库备份。

use crate::db::now_ts;
use crate::error::AppResult;
use crate::settings::Runtime;
use crate::vfs::Vfs;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(30 * 60);
const UPLOAD_SESSION_KEEP_SECS: i64 = 7 * 86_400;

async fn record_log(vfs: &Vfs, level: &str, category: &str, message: &str) {
    if let Err(error) = vfs.db.append_backend_log(level, category, message).await {
        tracing::warn!("记录维护日志失败: {error}");
    }
}

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
                record_log(
                    &vfs,
                    "error",
                    "维护",
                    &format!("维护任务失败，下轮重试：{error}"),
                )
                .await;
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
        record_log(
            vfs,
            "info",
            "维护",
            &format!("清理过期断点续传任务：释放 {} 条消息", abandoned.len()),
        )
        .await;
        vfs.uploader.delete_messages_best_effort(abandoned);
    }

    if extras.trash_retention_days > 0 {
        let freed = vfs
            .db
            .trash_purge_expired(extras.trash_retention_days)
            .await?;
        if !freed.is_empty() {
            tracing::info!("回收站超期清理：释放 {} 条消息", freed.len());
            record_log(
                vfs,
                "info",
                "维护",
                &format!("回收站超期清理：释放 {} 条消息", freed.len()),
            )
            .await;
            // 清理的是回收站里本就不可见的节点：可见树没有变化，不广播文件事件。
            vfs.uploader.delete_messages_best_effort(freed);
        }
    }

    if let Some(outcome) = crate::backup::automatic_if_due(vfs, runtime, &extras).await? {
        tracing::info!(
            file = %outcome.name,
            destinations = outcome.succeeded,
            "数据库自动备份完成"
        );
        let level = if outcome.failed > 0 { "warn" } else { "info" };
        record_log(
            vfs,
            level,
            "备份",
            &format!(
                "数据库自动备份{}：{}，成功 {} 个渠道，失败 {} 个",
                if outcome.failed > 0 {
                    "部分完成"
                } else {
                    "完成"
                },
                outcome.name,
                outcome.succeeded,
                outcome.failed
            ),
        )
        .await;
    }
    Ok(())
}
