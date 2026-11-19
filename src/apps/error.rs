//! 应用平台使用契约的统一错误，不向调用方泄露内部异常链。

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tgdrive_contract::{ApiError, ErrorResponse, error_code, error_http_status};

pub(crate) type PlatformResult<T> = Result<T, PlatformError>;

#[derive(Debug)]
pub(crate) struct PlatformError {
    pub code: String,
    pub message: String,
    pub details: Option<serde_json::Value>,
    pub retry_after_ms: Option<u32>,
}

impl PlatformError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
            retry_after_ms: None,
        }
    }

    pub fn field(mut self, field: &str) -> Self {
        self.details = Some(serde_json::json!({"field": field}));
        self
    }

    pub fn limited(code: &str, message: &str, retry_after_ms: u32) -> Self {
        Self {
            retry_after_ms: Some(retry_after_ms),
            ..Self::new(code, message)
        }
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}：{}", self.code, self.message)
    }
}
impl std::error::Error for PlatformError {}

impl From<tgdrive_contract::validation::ValidationError> for PlatformError {
    fn from(error: tgdrive_contract::validation::ValidationError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            details: Some(serde_json::json!({"field":error.field})),
            retry_after_ms: None,
        }
    }
}

impl From<sqlx::Error> for PlatformError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "应用平台数据库操作失败");
        Self::new(error_code::INTERNAL_ERROR, "宿主数据库操作失败")
    }
}
impl From<std::io::Error> for PlatformError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Self::new(error_code::NOT_FOUND, "内部对象不存在");
        }
        if error.raw_os_error() == Some(libc::ELOOP) {
            return Self::new(error_code::INVALID_PATH, "内部路径不能是链接");
        }
        tracing::error!(kind = ?error.kind(), "应用平台文件操作失败");
        Self::new(error_code::INTERNAL_ERROR, "宿主文件操作失败")
    }
}
impl From<serde_json::Error> for PlatformError {
    fn from(_: serde_json::Error) -> Self {
        Self::new(error_code::INVALID_REQUEST, "JSON 格式或字段无效")
    }
}

impl IntoResponse for PlatformError {
    fn into_response(self) -> Response {
        let request_id = super::opaque();
        let status = StatusCode::from_u16(error_http_status(&self.code).unwrap_or(500))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let retry = self.retry_after_ms;
        let mut response = (
            status,
            axum::Json(ErrorResponse {
                error: ApiError {
                    code: self.code,
                    message: self.message,
                    request_id: request_id.clone(),
                    details: self.details,
                    retry_after_ms: retry,
                },
            }),
        )
            .into_response();
        response.headers_mut().insert(
            "x-tgdrive-request-id",
            HeaderValue::from_str(&request_id).expect("随机标识为 ASCII"),
        );
        if let Some(ms) = retry {
            response.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&u64::from(ms).div_ceil(1000).to_string())
                    .expect("秒数为 ASCII"),
            );
        }
        response
    }
}
