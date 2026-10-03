use super::*;

#[tokio::test]
async fn revision_is_transactional_and_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.sqlite");
    let path = path.to_str().unwrap();
    let db = Db::open(path).await.unwrap();
    let original = db.reading_revision().await.unwrap();
    let node = db.mkdir(1, "目录").await.unwrap();
    let created = db.reading_revision().await.unwrap();
    assert_ne!(created, original);
    assert_eq!(created.len(), 32);
    sqlx::query("UPDATE nodes SET name=name WHERE id=?")
        .bind(node.id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.reading_revision().await.unwrap(), created);
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE nodes SET name='回滚' WHERE id=?")
        .bind(node.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(db.reading_revision().await.unwrap(), created);
    let reopened = Db::open(path).await.unwrap();
    assert_eq!(reopened.reading_revision().await.unwrap(), created);
    sqlx::query("DELETE FROM nodes WHERE id=?")
        .bind(node.id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_ne!(reopened.reading_revision().await.unwrap(), created);
}

#[tokio::test]
async fn persisted_format_index_is_used_for_candidate_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("index.sqlite").to_str().unwrap())
        .await
        .unwrap();
    let rows: Vec<(i64, i64, i64, String)> =
        sqlx::query_as("EXPLAIN QUERY PLAN SELECT id FROM reading_files WHERE format='epub'")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert!(
        rows.iter().any(|row| row.3.contains("idx_reading_format")),
        "{rows:?}"
    );
}
