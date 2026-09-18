//! 目录范围最小权限：空范围保持全盘；设置后所有应用文件能力必须落在范围内。
//! 路径均为规范化后的绝对路径（`/a/b`），根目录 `/` 等价于全盘。

use crate::{
    api::AppState,
    apps::InstalledApp,
    error::{AppError, AppResult},
};

fn under(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return true;
    }
    path == root || path.starts_with(&format!("{root}/"))
}

/// 范围内返回 true；空范围即全盘。
pub(super) fn in_scope(scope: &[String], path: &str) -> bool {
    scope.is_empty() || scope.iter().any(|root| under(root, path))
}

/// 逐次调用强制校验；范围变更不打断运行中的页面，下一次调用立即生效。
pub(super) async fn check(state: &AppState, app: &InstalledApp, path: &str) -> AppResult<()> {
    if in_scope(&app.scope, path) {
        return Ok(());
    }
    let _ = state;
    Err(AppError::Forbidden(
        "目录不在应用允许的访问范围内，请在应用设置中调整".into(),
    ))
}
