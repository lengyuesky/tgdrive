use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt as _;

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .extension(ConnectInfo(
            "127.0.0.1:12500".parse::<SocketAddr>().unwrap(),
        ));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test]
async fn upload_receipts_are_atomic_idempotent_and_scoped_tickets_are_revocable() {
    let (app, state, _dir) = crate::api::entries::tests::fixture().await;
    let db = &state.vfs.db;
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "fixture-only", "-1")
        .await
        .unwrap();
    let sha = "a".repeat(64);
    let info = db
        .create_upload_session(1, "empty.txt", "/empty.txt", 0, &sha, 16, target.id)
        .await
        .unwrap();
    let (_, login) = send(
        &app,
        "POST",
        "/api/auth/device/login",
        None,
        json!({"username":"entries-test","password":"entries-test-only","device_name":"传输测试"}),
    )
    .await;
    let token = login["access_token"].as_str().unwrap();
    let (status, legacy) = send(
        &app,
        "GET",
        &format!("/api/uploads/{}", info.session.id),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(legacy["session"]["id"], info.session.id);
    assert!(
        legacy.get("receipt").is_none(),
        "旧网页状态成功响应始终含会话"
    );
    let lookup = format!("/api/uploads/lookup?path=%2Fempty.txt&size=0&sha256={sha}");
    let (status, located) = send(&app, "GET", &lookup, Some(token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(located["id"], info.session.id);
    let (status, ticket) = send(
        &app,
        "POST",
        &format!("/api/uploads/{}/ticket", info.session.id),
        Some(token),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ticket}");
    let url = ticket["url"].as_str().unwrap();
    let (status, first) = send(&app, "POST", &format!("{url}/complete"), None, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["receipt"]["status"], "completed");
    let (status, second) = send(&app, "POST", &format!("{url}/complete"), None, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second, first);
    let (_, result) = send(&app, "GET", &format!("{url}/status"), None, Value::Null).await;
    assert_eq!(result["receipt"]["entry"]["id"], info.node.id);
    let (status, native_result) = send(
        &app,
        "GET",
        &format!("/api/uploads/{}/result", info.session.id),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(native_result, result);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/api/uploads/{}", info.session.id),
            Some(token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, located) = send(&app, "GET", &lookup, Some(token), Value::Null).await;
    assert!(located["id"].is_null(), "只读查询不得重建已完成会话");
    assert!(db.upload_session_info(&info.session.id).await.is_err());
    // 已完成回执重试不会改动后来覆盖同名文件的新节点。
    let replacement = db.create_uploading(1, "empty.txt").await.unwrap();
    db.finalize_upload(replacement, 0, Some(&"b".repeat(64)))
        .await
        .unwrap();
    db.complete_upload_session(&info.session.id).await.unwrap();
    assert_eq!(
        state.vfs.resolve("/empty.txt").await.unwrap().id,
        replacement
    );
    db.cancel_upload_session(&info.session.id).await.unwrap();
    assert_eq!(
        state.vfs.resolve("/empty.txt").await.unwrap().id,
        replacement
    );
    state.auth.rotate_secret(db).await.unwrap();
    assert_eq!(
        send(&app, "GET", &format!("{url}/status"), None, Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn cancelled_and_uncertain_uploads_cannot_be_replayed_as_success() {
    let (_app, state, _dir) = crate::api::entries::tests::fixture().await;
    let db = &state.vfs.db;
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "fixture-only", "-1")
        .await
        .unwrap();
    let info = db
        .create_upload_session(
            1,
            "file.txt",
            "/file.txt",
            3,
            &"a".repeat(64),
            16,
            target.id,
        )
        .await
        .unwrap();
    db.mark_upload_part_uncertain(&info.session.id, 0)
        .await
        .unwrap();
    let info = db.upload_session_info(&info.session.id).await.unwrap();
    assert_eq!(info.uncertain_parts, [0]);
    assert!(matches!(
        db.claim_upload_part(&info.session.id, 0).await,
        Err(AppError::TelegramUncertain(_))
    ));
    db.cancel_upload_session(&info.session.id).await.unwrap();
    db.cancel_upload_session(&info.session.id).await.unwrap();
    assert_eq!(
        db.upload_receipt(&info.session.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    assert!(matches!(
        db.complete_upload_session(&info.session.id).await,
        Err(AppError::Conflict(_))
    ));
}
#[tokio::test]
async fn restart_releases_claims_but_preserves_uncertainty() {
    let (_app, state, _dir) = crate::api::entries::tests::fixture().await;
    let db = &state.vfs.db;
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "fixture-only", "-1")
        .await
        .unwrap();
    let upload = db
        .create_upload_session(
            1,
            "restart.bin",
            "/restart.bin",
            3,
            &"a".repeat(64),
            3,
            target.id,
        )
        .await
        .unwrap();
    db.claim_upload_part(&upload.session.id, 0).await.unwrap();
    let during = db.upload_session_info(&upload.session.id).await.unwrap();
    assert_eq!(during.in_flight_parts, [0]);
    assert_eq!(during.uncertain_parts, [0]);
    db.reset_upload_part_claims().await.unwrap();
    let restarted = db.upload_session_info(&upload.session.id).await.unwrap();
    assert!(restarted.in_flight_parts.is_empty());
    assert_eq!(restarted.uncertain_parts, [0]);
    assert!(matches!(
        db.claim_upload_part(&upload.session.id, 0).await,
        Err(AppError::TelegramUncertain(_))
    ));
}

#[tokio::test]
async fn upload_conflict_policy_is_persistent_and_checked_again_at_commit() {
    let (_app, state, _dir) = crate::api::entries::tests::fixture().await;
    let db = &state.vfs.db;
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "fixture-only", "-1")
        .await
        .unwrap();
    let hash = "a".repeat(64);
    let session = db
        .create_upload_session_with_policy(
            1,
            "race.txt",
            "/race.txt",
            0,
            &hash,
            16,
            target.id,
            false,
        )
        .await
        .unwrap();
    assert!(!session.session.overwrite);
    assert!(matches!(
        db.create_upload_session_with_policy(
            1,
            "race.txt",
            "/race.txt",
            0,
            &hash,
            16,
            target.id,
            true
        )
        .await,
        Err(AppError::Conflict(_))
    ));
    assert!(
        !db.upload_session_info(&session.session.id)
            .await
            .unwrap()
            .session
            .overwrite
    );
    // 开始上传后另一条独立管线已提交同名文件，完成时必须拒绝覆盖。
    let replacement = db
        .create_upload_session(
            1,
            "race.txt",
            "/race.txt",
            0,
            &"b".repeat(64),
            16,
            target.id,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(replacement, AppError::BadRequest(_)),
        "普通入口应拒绝与另一内容会话冲突"
    );
    let other = db.create_uploading(1, "replacement.tmp").await.unwrap();
    db.finalize_upload(other, 0, Some(&"b".repeat(64)))
        .await
        .unwrap();
    db.move_node(other, 1, "race.txt").await.unwrap();
    let replacement = other;
    assert!(matches!(
        db.complete_upload_session(&session.session.id).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        state.vfs.resolve("/race.txt").await.unwrap().id,
        replacement
    );
    assert!(
        db.upload_receipt(&session.session.id)
            .await
            .unwrap()
            .is_none()
    );
    db.cancel_upload_session(&session.session.id).await.unwrap();
    assert_eq!(
        state.vfs.resolve("/race.txt").await.unwrap().id,
        replacement
    );
    assert!(matches!(
        db.create_upload_session_with_policy(
            1,
            "race.txt",
            "/race.txt",
            0,
            &hash,
            16,
            target.id,
            false
        )
        .await,
        Err(AppError::Conflict(_))
    ));
    // 省略覆盖策略仍保持旧网页 true 的行为。
    let legacy = db
        .create_upload_session(1, "race.txt", "/race.txt", 0, &hash, 16, target.id)
        .await
        .unwrap();
    assert!(legacy.session.overwrite);
    db.complete_upload_session(&legacy.session.id)
        .await
        .unwrap();
    assert_eq!(
        state.vfs.resolve("/race.txt").await.unwrap().id,
        legacy.node.id
    );
}

#[tokio::test]
async fn instant_conflict_policy_prevents_replacement_and_keeps_legacy_idempotence() {
    let (app, state, _dir) = crate::api::entries::tests::fixture().await;
    let db = &state.vfs.db;
    let hash = "a".repeat(64);
    let source = db.create_uploading(1, "source.txt").await.unwrap();
    db.finalize_upload(source, 0, Some(&hash)).await.unwrap();
    let existing = db.create_uploading(1, "target.txt").await.unwrap();
    db.finalize_upload(existing, 0, Some(&"b".repeat(64)))
        .await
        .unwrap();
    let (_, login) = send(&app, "POST", "/api/auth/device/login", None, json!({"username":"entries-test","password":"entries-test-only","device_name":"冲突策略测试"})).await;
    let token = login["access_token"].as_str().unwrap();
    let (status, error) = send(
        &app,
        "POST",
        "/api/fs/instant",
        Some(token),
        json!({"dir":"/","name":"target.txt","size":0,"sha256":hash,"overwrite":false}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(state.vfs.resolve("/target.txt").await.unwrap().id, existing);
    let (status, value) = send(
        &app,
        "POST",
        "/api/fs/instant",
        Some(token),
        json!({"dir":"/","name":"target.txt","size":0,"sha256":hash}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["linked"], true);
    let linked = state.vfs.resolve("/target.txt").await.unwrap();
    assert_ne!(linked.id, existing);
    assert_eq!(linked.sha256.as_deref(), Some(hash.as_str()));
    assert_eq!(
        db.instant_link(&hash, 0, 1, "target.txt")
            .await
            .unwrap()
            .unwrap()
            .0,
        linked.id
    );
    let directory = db.mkdir(1, "directory.txt").await.unwrap();
    assert!(matches!(
        db.instant_link_with_policy(&hash, 0, 1, "directory.txt", true)
            .await,
        Err(AppError::IsDirectory)
    ));
    assert_eq!(
        state.vfs.resolve("/directory.txt").await.unwrap().id,
        directory.id
    );
}

#[tokio::test]
async fn upload_commit_rejects_trashed_parent_and_can_resume_after_restore() {
    let (_app, state, _dir) = crate::api::entries::tests::fixture().await;
    let db = &state.vfs.db;
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "fixture-only", "-1")
        .await
        .unwrap();
    let parent = db.mkdir(1, "parent").await.unwrap();
    let child = db.mkdir(parent.id, "child").await.unwrap();
    let upload = db
        .create_upload_session_with_policy(
            child.id,
            "empty.txt",
            "/parent/child/empty.txt",
            0,
            &"a".repeat(64),
            16,
            target.id,
            false,
        )
        .await
        .unwrap();
    db.trash_node(parent.id, "/parent").await.unwrap();
    assert!(matches!(
        db.complete_upload_session(&upload.session.id).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(db.node(upload.node.id).await.unwrap().status, "uploading");
    assert!(
        db.upload_receipt(&upload.session.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(state.vfs.resolve("/parent/child/empty.txt").await.is_err());
    assert_eq!(db.trash_restore(parent.id).await.unwrap(), "/parent");
    db.complete_upload_session(&upload.session.id)
        .await
        .unwrap();
    assert_eq!(
        state
            .vfs
            .resolve("/parent/child/empty.txt")
            .await
            .unwrap()
            .id,
        upload.node.id
    );
    assert_eq!(
        db.upload_receipt(&upload.session.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "completed"
    );
}

#[tokio::test]
async fn concurrent_complete_and_cancel_never_delete_committed_content() {
    let (_app, state, _dir) = crate::api::entries::tests::fixture().await;
    let db = &state.vfs.db;
    let target = db
        .ensure_storage_target("http://127.0.0.1:1", "fixture-only", "-1")
        .await
        .unwrap();
    let info = db
        .create_upload_session(
            1,
            "empty.txt",
            "/empty.txt",
            0,
            &"a".repeat(64),
            16,
            target.id,
        )
        .await
        .unwrap();
    let (complete, cancel) = tokio::join!(
        db.complete_upload_session(&info.session.id),
        db.cancel_upload_session(&info.session.id)
    );
    assert!(cancel.is_ok());
    let receipt = db.upload_receipt(&info.session.id).await.unwrap().unwrap();
    if receipt.status == "completed" {
        assert!(complete.is_ok());
        assert_eq!(
            state.vfs.resolve("/empty.txt").await.unwrap().id,
            info.node.id
        );
    } else {
        assert!(complete.is_err());
        assert!(state.vfs.resolve("/empty.txt").await.is_err());
    }
}
