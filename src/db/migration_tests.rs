//! 删除应用平台的迁移回归：历史库仍能升级，网盘本体的行与对象逐项保持不变。

use sqlx::{Connection, SqliteConnection};
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) async fn legacy_database(path: &Path) -> SqliteConnection {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::migrate!("./migrations")
        .run_direct(Some(27), &mut conn, false)
        .await
        .unwrap();
    conn
}

fn application_object(name: &str, owner: &str) -> bool {
    owner == "apps"
        || owner.starts_with("app_")
        || owner == "reading_tree_revision"
        || name.starts_with("reading_")
        || name == "idx_reading_format"
}

async fn host_schema(conn: &mut SqliteConnection) -> Vec<(String, String, String)> {
    let objects: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name",
    )
    .fetch_all(conn)
    .await
    .unwrap();
    objects
        .into_iter()
        .filter(|(_, name, owner, _)| !application_object(name, owner))
        .map(|(kind, name, _, sql)| (kind, name, sql))
        .collect()
}

async fn host_rows(conn: &mut SqliteConnection) -> BTreeMap<String, Vec<String>> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name!='_sqlx_migrations' ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let mut result = BTreeMap::new();
    for table in tables {
        if application_object(&table, &table) {
            continue;
        }
        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> = sqlx::query_as(
            sqlx::AssertSqlSafe(format!("PRAGMA table_info(\"{table}\")")),
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        let fields = columns
            .iter()
            .map(|(_, name, _, _, _, _)| format!("quote(\"{name}\")"))
            .collect::<Vec<_>>()
            .join(" || ',' || ");
        let mut rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {fields} FROM \"{table}\""
        )))
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        rows.sort();
        result.insert(table, rows);
    }
    result
}

#[tokio::test]
async fn removal_preserves_all_host_rows_and_objects() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = legacy_database(&dir.path().join("legacy.db")).await;
    sqlx::raw_sql(
        "INSERT INTO nodes(id,parent_id,name,is_dir,size,status,created_at,modified_at,favorite) VALUES
            (2,1,'file.bin',0,8,'ready',1,1,1),
            (3,1,'trash.bin',0,8,'trashed',1,1,0),
            (4,1,'upload.bin',0,8,'uploading',1,1,0);
         INSERT INTO storage_targets(id,api_base,bot_token,chat_id,created_at)
            VALUES(1,'https://api.invalid','test-token','test-channel',1);
         INSERT INTO chunks(node_id,idx,size,tg_file_id,tg_message_id,storage_target_id)
            VALUES(2,0,8,'file-id',42,1);
         INSERT INTO shares(token,node_id,password,created_at) VALUES('share',2,'code',1);
         INSERT INTO trash VALUES(3,1,'/trash.bin',1);
         INSERT INTO settings VALUES('must_survive','yes');
         INSERT INTO device_sessions(id,name,auth_epoch,created_at,last_used_at,expires_at)
            VALUES('device','测试设备','epoch',1,1,9999999999);
         INSERT INTO device_refresh_tokens VALUES('refresh','device',NULL);
         INSERT INTO inbox_requests(id,bot_id,chat_id,source_message_id,media_kind,original_name,
            original_suffix,name,file_id,file_size,created_at,updated_at,expires_at)
            VALUES('inbox','bot',1,1,'document','inbox.bin','bin','inbox.bin','inbox-file',8,1,1,9999999999);
         INSERT INTO upload_sessions(id,node_id,display_path,expected_size,sha256,chunk_size,
            storage_target_id,created_at,updated_at)
            VALUES('upload',4,'/upload.bin',8,'hash',8,1,1,1);
         INSERT INTO apps(id,manifest,version,digest,revision,package,source,installed_at,updated_at)
            VALUES('legacy-app','{}','1','digest','revision',X'01','test',1,1);
         INSERT INTO app_settings VALUES('legacy-app','{}');
         INSERT INTO app_data VALUES('legacy-app','key','value','revision',1);
         INSERT INTO app_recovery VALUES('legacy-app','{}','1','digest',X'01','test','{}','{}',1,1);
         INSERT INTO app_lifecycle(app_id,updated_at) VALUES('legacy-app',1);
         INSERT INTO app_scraper_batches(id,app_id,revision,config_key,root_id,root_path,owner,generation,created_at,updated_at)
            VALUES('batch','legacy-app','revision','key',1,'/','owner','generation',1,1);
         INSERT INTO app_scraper_sources VALUES('batch','source','id','{}');
         INSERT INTO app_scraper_items(id,batch_id,group_key,position,files)
            VALUES('item','batch','group',0,'[]');
         INSERT INTO app_scraper_directories VALUES('batch','/',1);
         INSERT INTO app_scraper_upload_guards VALUES(4,'batch','item','role','{}');",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    let rows = host_rows(&mut conn).await;
    let objects = host_schema(&mut conn).await;
    let history: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut conn)
            .await
            .unwrap();

    // 此测试专门证明 0028 的删除迁移；后续新平台允许给 nodes 增加通用索引。
    sqlx::migrate!("./migrations")
        .run_direct(Some(28), &mut conn, false)
        .await
        .unwrap();

    assert_eq!(host_rows(&mut conn).await, rows, "网盘本体的数据行不能改变");
    assert_eq!(
        host_schema(&mut conn).await,
        objects,
        "网盘本体的对象不能改变"
    );
    let remaining: Vec<(String, String)> =
        sqlx::query_as("SELECT name,tbl_name FROM sqlite_master")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert!(
        remaining
            .iter()
            .all(|(name, owner)| !application_object(name, owner))
    );
    let preserved: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version<=27 ORDER BY version",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(preserved, history, "旧迁移校验和不能改变");
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut conn)
            .await
            .unwrap()
            .is_none()
    );
    conn.close().await.unwrap();
}
