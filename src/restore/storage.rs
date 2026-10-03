//! 本模块只在实例锁保护下执行离线切库。恢复文件绝不与活跃连接池交叠。
use super::*;
use sqlx::{Connection, Row, SqliteConnection};
use std::fs::{File, OpenOptions};
use std::io::Read;

pub fn require_cli_idle(db_path: &str, is_check: bool) -> anyhow::Result<()> {
    let journal = load(&root(db_path)?)?;
    anyhow::ensure!(
        journal.stage == "done" && (!is_check || !journal.status.safe_mode),
        "存在恢复标记或处于安全模式，请先启动服务处理恢复；CLI 不得绕过恢复保护"
    );
    Ok(())
}

pub struct InstanceLock {
    _file: File,
}
impl InstanceLock {
    pub fn acquire(db_path: &str) -> anyhow::Result<Self> {
        let path = database_path(db_path)?;
        let lock = PathBuf::from(format!("{}.instance-lock", path.display()));
        let file = private_open(&lock, false)?;
        file.try_lock()
            .map_err(|e| anyhow::anyhow!("数据库已有运行实例或写库 CLI，不能同时打开：{e}"))?;
        Ok(Self { _file: file })
    }
}
fn database_path(db_path: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(db_path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let parent = std::fs::canonicalize(parent)?;
    let path = parent.join(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("数据库路径无效"))?,
    );
    // 禁止软链接避免同一数据库以两个锁路径打开。
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        anyhow::ensure!(!meta.file_type().is_symlink(), "数据库文件不能是符号链接");
    }
    Ok(path)
}
pub(super) fn root(db_path: &str) -> anyhow::Result<PathBuf> {
    let db = database_path(db_path)?;
    let root = PathBuf::from(format!("{}.restore", db.display()));
    if let Ok(meta) = std::fs::symlink_metadata(&root) {
        anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "恢复状态目录无效"
        );
    }
    std::fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(root)
}
fn private_open(path: &Path, create_new: bool) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).read(true);
    if create_new {
        opts.create_new(true);
    } else {
        opts.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}
fn sync_dir(path: &Path) -> anyhow::Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
pub(super) fn load(root: &Path) -> anyhow::Result<Journal> {
    let path = root.join("state.json");
    match File::open(path) {
        Ok(file) => {
            anyhow::ensure!(
                file.metadata()?.len() <= 1024 * 1024,
                "恢复状态文件超限，拒绝启动以避免旧队列运行"
            );
            let journal: Journal = serde_json::from_reader(file)?;
            anyhow::ensure!(
                [
                    "done",
                    "preparing",
                    "ready",
                    "pending",
                    "snapshotting",
                    "switching",
                    "verifying",
                    "rollback",
                    "resume"
                ]
                .contains(&journal.stage.as_str()),
                "恢复阶段未知，拒绝启动"
            );
            Ok(journal)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn save(root: &Path, journal: &Journal) -> anyhow::Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix("state-")
        .tempfile_in(root)?;
    serde_json::to_writer(&mut temp, journal)?;
    temp.as_file().sync_all()?;
    temp.persist(root.join("state.json"))?;
    sync_dir(root)
}
pub(super) fn install_copy(source: &Path, dest: &Path) -> anyhow::Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("目标目录无效"))?;
    let mut temp = tempfile::Builder::new()
        .prefix("install-")
        .tempfile_in(parent)?;
    std::io::copy(&mut File::open(source)?, &mut temp)?;
    temp.as_file().sync_all()?;
    temp.persist(dest)?;
    sync_dir(parent)
}
pub(super) fn remove_candidate(root: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = root.join(format!("candidate.db{suffix}"));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    // 清理上次进程崩溃遗留的下载明文，只遍历本模块生成的目录。
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("download-")
            && entry.file_type()?.is_dir()
        {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    sync_dir(root)
}
pub(super) async fn digest(path: &Path) -> anyhow::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut sha = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
    }
    Ok(crate::engine::upload::to_hex(&sha.finalize()))
}
fn options(path: &Path, readonly: bool) -> sqlx::sqlite::SqliteConnectOptions {
    sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(readonly)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10))
        .pragma("trusted_schema", "OFF")
        .pragma("query_only", if readonly { "ON" } else { "OFF" })
}
async fn integrity(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<(String,)> = sqlx::query_as("PRAGMA integrity_check(1)")
        .fetch_all(&mut *conn)
        .await?;
    anyhow::ensure!(
        rows.len() == 1 && rows[0].0 == "ok",
        "SQLite 完整性检查失败"
    );
    let foreign = sqlx::query("PRAGMA foreign_key_check")
        .fetch_optional(&mut *conn)
        .await?;
    anyhow::ensure!(foreign.is_none(), "SQLite 外键检查失败");
    Ok(())
}

/// 初检只读连接，不运行 Db::open 或打开 covers 旁路库。迁移仅作用于私有下载副本。
pub(super) async fn validate_candidate(path: &Path) -> anyhow::Result<(i64, i64)> {
    let mut header = [0u8; 16];
    File::open(path)?.read_exact(&mut header)?;
    anyhow::ensure!(&header == b"SQLite format 3\0", "不是 SQLite 3 数据库");
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= MAX_BYTES,
        "数据库超过 8 GiB 上限"
    );
    // 只在内存库执行可信迁移；候选库里的可执行 schema 必须逐字匹配内置定义。
    // 不能仅按对象名称放行，否则同名恶意触发器可能在后续迁移时执行。
    let mut reference = SqliteConnection::connect("sqlite::memory:").await?;
    sqlx::migrate!("./migrations")
        .run_direct(None, &mut reference, false)
        .await?;
    let expected_schema = schema(&mut reference).await?;
    reference.close().await?;
    let mut conn = SqliteConnection::connect_with(&options(path, true)).await?;
    let result = async {
        integrity(&mut conn).await?;
        let executable_schema: Vec<(String, String, String)> = sqlx::query_as("SELECT type,name,sql FROM sqlite_master WHERE type IN ('trigger','view') OR upper(sql) LIKE '%VIRTUAL TABLE%'").fetch_all(&mut conn).await?;
        anyhow::ensure!(executable_schema.iter().all(|object| expected_schema.contains(object)), "备份包含不受支持或被篡改的触发器、视图或虚拟表");
        for name in ["nodes", "chunks", "settings", "_sqlx_migrations"] {
            let row: Option<(String,)> = sqlx::query_as("SELECT type FROM sqlite_master WHERE name = ?").bind(name).fetch_optional(&mut conn).await?;
            anyhow::ensure!(row.as_ref().is_some_and(|r| r.0 == "table"), "备份缺少必要表 {name}");
        }
        let applied = sqlx::query("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version").fetch_all(&mut conn).await?;
        let migrator = sqlx::migrate!("./migrations");
        let known: Vec<_> = migrator.iter().filter(|m| !m.migration_type.is_down_migration()).collect();
        anyhow::ensure!(!applied.is_empty() && applied.len() <= known.len(), "备份迁移版本不存在或比当前程序更新");
        for (row, expected) in applied.iter().zip(&known) {
            let version: i64 = row.try_get("version")?;
            let success: bool = row.try_get("success")?;
            let checksum: Vec<u8> = row.try_get("checksum")?;
            anyhow::ensure!(version == expected.version && success && checksum == expected.checksum.as_ref(), "备份迁移记录不连续、未成功或校验和不匹配（版本 {version}）");
        }
        Ok::<(), anyhow::Error>(())
    }.await;
    conn.close().await?;
    result?;
    let mut conn = SqliteConnection::connect_with(&options(path, false)).await?;
    let result = async {
        sqlx::migrate!("./migrations").run_direct(None, &mut conn, false).await?;
        integrity(&mut conn).await?;
        // 校验迁移后的完整 schema，不能只信可伪造的迁移记录。
        let actual = schema(&mut conn).await?;
        anyhow::ensure!(actual == expected_schema, "备份数据库结构与本版本迁移结果不匹配");
        let root: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM nodes WHERE id = 1 AND is_dir = 1 AND parent_id IS NULL").fetch_one(&mut conn).await?;
        anyhow::ensure!(root.0 == 1, "备份根目录不存在或已损坏");
        let (files, directories): (i64, i64) = sqlx::query_as("SELECT COALESCE(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END),0), COALESCE(SUM(CASE WHEN is_dir = 1 THEN 1 ELSE 0 END),0) FROM nodes").fetch_one(&mut conn).await?;
        checkpoint(&mut conn).await?;
        Ok::<_, anyhow::Error>((files, directories))
    }.await;
    conn.close().await?;
    result
}
async fn schema(conn: &mut SqliteConnection) -> anyhow::Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name",
    )
    .fetch_all(conn)
    .await?)
}
async fn checkpoint(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let (busy, _, _): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(&mut *conn)
        .await?;
    anyhow::ensure!(busy == 0, "WAL checkpoint busy，禁止切换数据库");
    Ok(())
}

pub(super) async fn preserve_settings(candidate: &Path, state: &AppState) -> anyhow::Result<()> {
    let cfg = state.rt.current();
    let security_keys = [
        crate::auth::TOTP_SECRET_KEY,
        crate::auth::TOTP_RECOVERY_KEY,
        crate::auth::WEBDAV_PASSWORD_KEY,
    ];
    let old = state.vfs.db.settings_get(&security_keys).await?;
    let password = if crate::password::is_hash(&cfg.password) {
        cfg.password.clone()
    } else {
        crate::password::hash(&cfg.password)?
    };
    let mut values: Vec<(String, String)> = old.into_iter().collect();
    values.extend([
        ("cfg.username".into(), cfg.username.clone()),
        ("cfg.password".into(), password),
        (crate::auth::SECRET_KEY.into(), crate::auth::random_secret()),
        ("cfg.proxy_enabled".into(), cfg.proxy.enabled.to_string()),
        (
            "cfg.proxy_provider".into(),
            cfg.proxy.provider.as_str().into(),
        ),
        ("cfg.proxy_url".into(), cfg.proxy.url.clone()),
        ("cfg.telegram_api_id".into(), cfg.telegram_api_id.clone()),
        (
            "cfg.telegram_api_hash".into(),
            cfg.telegram_api_hash.clone(),
        ),
        (
            "cfg.tmp_dir".into(),
            cfg.tmp_dir.to_string_lossy().to_string(),
        ),
        // 不将当前远端密码套用到备份中的其他目标，也不能复活备份中的旧密码。
        ("extra.backup_webdav_password".into(), String::new()),
        ("extra.backup_s3_secret_key".into(), String::new()),
        ("extra.backup_s3_session_token".into(), String::new()),
    ]);
    let mut conn = SqliteConnection::connect_with(&options(candidate, false)).await?;
    let result = async {
        let mut tx = conn.begin().await?;
        sqlx::query("DELETE FROM settings WHERE k LIKE 'auth.%' OR k LIKE 'mtproto.session.%' OR k = 'secret'").execute(&mut *tx).await?;
        for (key, value) in values {
            sqlx::query("INSERT INTO settings(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v").bind(key).bind(value).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        checkpoint(&mut conn).await?;
        Ok::<(), anyhow::Error>(())
    }.await;
    conn.close().await?;
    result?;
    File::open(candidate)?.sync_all()?;
    sync_dir(candidate.parent().unwrap())
}

/// 持锁且没有任何 Db/worker 时调用。任意切换中断都回滚，不重复应用候选。
pub async fn startup(env: &crate::config::Config) -> anyhow::Result<bool> {
    let db_path = database_path(&env.db_path)?;
    let root = root(&env.db_path)?;
    let mut journal = load(&root)?;
    match journal.stage.as_str() {
        "preparing" | "ready" => {
            remove_candidate(&root)?;
            journal.stage = "done".into();
            journal.status.phase = "cancelled".into();
            journal.status.message = Some("上次准备任务因进程重启失效，请重新准备".into());
            save(&root, &journal)?;
        }
        "resume" => {
            journal.stage = "done".into();
            journal.status.phase = "succeeded".into();
            journal.status.safe_mode = false;
            journal.status.message = Some("安全模式已解除，后台任务重新启用".into());
            save(&root, &journal)?;
        }
        "switching" | "verifying" | "rollback" => {
            rollback(
                &root,
                &db_path,
                &mut journal,
                "检测到上次恢复切换中断，已恢复原数据库",
            )
            .await?;
        }
        "pending" | "snapshotting" => {
            let result = apply(&root, &db_path, &mut journal, env).await;
            if let Err(error) = result {
                // 以已落盘阶段为准，不能把一次失败的 save 当成已经提交。
                journal = load(&root)?;
                if matches!(
                    journal.stage.as_str(),
                    "switching" | "verifying" | "rollback"
                ) {
                    rollback(
                        &root,
                        &db_path,
                        &mut journal,
                        &format!("新数据库验证失败，已回滚：{error}"),
                    )
                    .await?;
                } else if journal.stage == "done" {
                    // 成功状态已持久化，仅暂存清理失败；不能把已经应用的库报告为未改变。
                    tracing::warn!("恢复已完成，暂存清理失败：{error}");
                } else {
                    journal.stage = "done".into();
                    journal.status.phase = "failed".into();
                    journal.status.message = Some(format!("恢复未应用，原库未改变：{error}"));
                    save(&root, &journal)?;
                    remove_candidate(&root)?;
                }
            }
        }
        _ => {}
    }
    Ok(journal.status.safe_mode)
}

async fn apply(
    root: &Path,
    db_path: &Path,
    journal: &mut Journal,
    env: &crate::config::Config,
) -> anyhow::Result<()> {
    let candidate = root.join("candidate.db");
    anyhow::ensure!(
        journal.digest.as_deref() == Some(&digest(&candidate).await?),
        "候选摘要不一致"
    );
    // pending 阶段发生崩溃可安全重做快照，仍未替换旧库。
    journal.stage = "snapshotting".into();
    if journal.status.rollback_path.is_none() {
        // 单独保留每一次恢复前副本，不覆盖更早的回滚文件。
        let name = format!("rollback-{}-{}.db", now_ts(), crate::auth::random_secret());
        journal.status.rollback_path = Some(root.join(name).to_string_lossy().to_string());
    }
    save(root, journal)?;
    let rollback_path = checked_rollback_path(root, journal)?;
    if rollback_path.exists() {
        std::fs::remove_file(&rollback_path)?;
    }
    let mut conn = SqliteConnection::connect_with(&options(db_path, false)).await?;
    let result = async {
        // VACUUM INTO 是一致性快照，包含已提交 WAL；不能用 fs::copy 复制运行时主文件。
        sqlx::query("VACUUM INTO ?")
            .bind(rollback_path.to_string_lossy().as_ref())
            .execute(&mut conn)
            .await?;
        checkpoint(&mut conn).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    conn.close().await?;
    result?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rollback_path, std::fs::Permissions::from_mode(0o600))?;
    }
    File::open(&rollback_path)?.sync_all()?;
    sync_dir(root)?;
    // 必须先 durable 切换标记，随后任何崩溃一律从完整快照回滚。
    journal.stage = "switching".into();
    save(root, journal)?;
    remove_sidecars(db_path)?;
    install_copy(&candidate, db_path)?;
    journal.stage = "verifying".into();
    save(root, journal)?;
    validate_startup(db_path, env).await?;
    journal.stage = "done".into();
    journal.status.phase = "succeeded".into();
    journal.status.safe_mode = true;
    journal.status.message = Some("恢复成功。请重新登录；安全模式下历史上传清理、删除队列、迁移、备份与收件均暂停。确认历史任务安全后再恢复后台任务。".into());
    journal
        .status
        .warnings
        .push("远端 WebDAV/S3 备份密码已清空；未把当前密码套用到备份中的其他目标。".into());
    save(root, journal)?;
    remove_candidate(root)?;
    Ok(())
}
async fn validate_startup(db_path: &Path, env: &crate::config::Config) -> anyhow::Result<()> {
    let db = Db::open(
        db_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("数据库路径编码无效"))?,
    )
    .await?;
    db.set_background_blocked(true);
    let result = async {
        let cfg = crate::settings::RuntimeCfg::load(&db, env).await?;
        anyhow::ensure!(
            !cfg.username.is_empty() && crate::password::is_hash(&cfg.password),
            "恢复后的管理员配置不合法"
        );
        let mtproto = crate::mtproto::MtprotoConfig::from_values(
            db.clone(),
            &cfg.telegram_api_id,
            &cfg.telegram_api_hash,
        )?;
        let targets = db.storage_targets().await?;
        TgRegistry::new(&targets, None, vec![], mtproto, cfg.proxy.clone())?;
        crate::auth::Auth::init(
            &cfg.username,
            &cfg.password,
            env.secret.as_deref(),
            env.cookie_secure,
            &db,
        )
        .await?;
        crate::extras::load(&db).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    db.close().await;
    result
}
fn checked_rollback_path(root: &Path, journal: &Journal) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(
        journal
            .status
            .rollback_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("缺少回滚快照路径，禁止启动旧队列"))?,
    );
    anyhow::ensure!(
        path.parent() == Some(root)
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("rollback-")),
        "回滚路径不在私有恢复目录内"
    );
    Ok(path)
}
fn remove_sidecars(db: &Path) -> anyhow::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    sync_dir(db.parent().unwrap())
}
async fn rollback(
    root: &Path,
    db_path: &Path,
    journal: &mut Journal,
    reason: &str,
) -> anyhow::Result<()> {
    let path = checked_rollback_path(root, journal)?;
    let mut conn = SqliteConnection::connect_with(&options(&path, true)).await?;
    let result = integrity(&mut conn).await;
    conn.close().await?;
    result?;
    journal.stage = "rollback".into();
    save(root, journal)?;
    remove_sidecars(db_path)?;
    install_copy(&path, db_path)?;
    journal.stage = "done".into();
    journal.status.phase = "rolled_back".into();
    journal.status.safe_mode = journal.previous_safe_mode;
    journal.status.message = Some(reason.to_string());
    save(root, journal)?;
    remove_candidate(root)?;
    Ok(())
}
