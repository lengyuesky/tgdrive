//! 统一错误类型:业务错误 → HTTP 状态码映射

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("路径不存在")]
    NotFound,
    #[error("目标已存在")]
    AlreadyExists,
    #[error("{0}")]
    Conflict(String),
    #[error("阅读记录已被另一页面更新，请选择使用已保存进度或从当前页继续")]
    AppDataConflict,
    /// 应用运行会话过期；宿主前端凭 `session_expired` 码静默续签后重试。
    #[error("应用会话已过期，请重新打开")]
    SessionExpired,
    /// 应用停用、卸载或更新导致修订轮换；宿主凭 `app_changed` 码提示重开。
    #[error("应用已停用、卸载或更新，请重新打开")]
    AppChanged,
    #[error("非法路径: {0}")]
    InvalidPath(String),
    #[error("目标是目录")]
    IsDirectory,
    #[error("目标不是目录")]
    NotDirectory,
    #[error("未认证")]
    Unauthorized,
    /// 密码正确但还需要两步验证码；前端凭 `totp_required` 码显示验证码输入框。
    #[error("请输入两步验证码")]
    TotpRequired,
    #[error("两步验证码错误或已使用")]
    TotpInvalid,
    #[error("{0}")]
    Forbidden(String),
    #[error("请求无效: {0}")]
    BadRequest(String),
    #[error("请求过于频繁，请在 {0} 秒后重试")]
    RateLimited(u64),
    #[error("Telegram API 错误: {0}")]
    Telegram(String),
    #[error("Telegram API 明确拒绝: {0}")]
    TelegramRejected(String),
    #[error("Telegram 上传结果不确定（为避免重复未自动重试）: {0}")]
    TelegramUncertain(String),
    #[error("上传服务已重启，正在重试")]
    UploadInterrupted,
    #[error("数据库错误: {0}")]
    Db(sqlx::Error),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

/// 唯一约束冲突多是并发同名创建（先查后写之间被抢先），属于客户端可理解的
/// 冲突而不是服务故障，统一映射为「已存在」(409)。
impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::Database(db) if db.is_unique_violation() => AppError::AlreadyExists,
            _ => AppError::Db(error),
        }
    }
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::AlreadyExists => StatusCode::CONFLICT,
            AppError::Conflict(_) | AppError::AppDataConflict => StatusCode::CONFLICT,
            AppError::InvalidPath(_) | AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            AppError::IsDirectory | AppError::NotDirectory => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Unauthorized | AppError::TotpRequired | AppError::TotpInvalid => {
                StatusCode::UNAUTHORIZED
            }
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::SessionExpired => StatusCode::FORBIDDEN,
            AppError::AppChanged => StatusCode::CONFLICT,
            AppError::Telegram(_)
            | AppError::TelegramRejected(_)
            | AppError::TelegramUncertain(_) => StatusCode::BAD_GATEWAY,
            AppError::UploadInterrupted => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Db(_) | AppError::Io(_) | AppError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl AppError {
    /// 面向客户端的稳定错误信息与机器码。数据库、IO 与网络错误可能携带本地路径或
    /// 上游 URL：详细信息只进服务端日志，客户端拿到不泄密的提示。
    pub fn public_parts(&self) -> (String, Option<&'static str>) {
        let status = self.status();
        let message = if matches!(self, AppError::UploadInterrupted) {
            "上传服务已重启，正在重试".to_string()
        } else if status.is_server_error() {
            match self {
                AppError::Telegram(_)
                | AppError::TelegramRejected(_)
                | AppError::TelegramUncertain(_) => "上游存储暂时不可用".to_string(),
                _ => "服务内部错误".to_string(),
            }
        } else {
            self.to_string()
        };
        let code = match self {
            AppError::UploadInterrupted => Some("upload_interrupted"),
            AppError::AppDataConflict => Some("storage_conflict"),
            AppError::SessionExpired => Some("session_expired"),
            AppError::AppChanged => Some("app_changed"),
            AppError::TotpRequired => Some("totp_required"),
            AppError::TotpInvalid => Some("totp_invalid"),
            _ => None,
        };
        (message, code)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let retry_after = match &self {
            AppError::RateLimited(seconds) => Some(*seconds),
            _ => None,
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "请求处理失败");
        }
        let (public_error, code) = self.public_parts();
        let mut response = (
            status,
            Json(serde_json::json!({ "error": public_error, "code": code })),
        )
            .into_response();
        if let Some(seconds) = retry_after
            && let Ok(value) = seconds.to_string().parse()
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}

pub type AppResult<T> = Result<T, AppError>;
