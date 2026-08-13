//! 统一错误类型:业务错误 → HTTP 状态码映射

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("路径不存在")]
    NotFound,
    #[error("目标已存在")]
    AlreadyExists,
    #[error("非法路径: {0}")]
    InvalidPath(String),
    #[error("目标是目录")]
    IsDirectory,
    #[error("目标不是目录")]
    NotDirectory,
    #[error("未认证")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("请求无效: {0}")]
    BadRequest(String),
    #[error("Telegram API 错误: {0}")]
    Telegram(String),
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::AlreadyExists => StatusCode::CONFLICT,
            AppError::InvalidPath(_) | AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::IsDirectory | AppError::NotDirectory => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::Telegram(_) => StatusCode::BAD_GATEWAY,
            AppError::Db(_) | AppError::Io(_) | AppError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "请求处理失败");
        }
        (status, Json(serde_json::json!({ "error": self.to_string() }))).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
