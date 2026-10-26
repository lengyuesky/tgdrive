-- v22：原生客户端身份与设备会话。凭据表不触发用户数据修订，不进入远程备份。
CREATE TABLE client_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    server_id TEXT NOT NULL
);
INSERT INTO client_identity VALUES (1, lower(hex(randomblob(16))));

CREATE TABLE device_sessions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    auth_epoch TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE INDEX device_sessions_expiry ON device_sessions(expires_at);

-- 轮换后保留已消费的哈希；任何旧刷新凭据重放都会撤销整个设备会话。
CREATE TABLE device_refresh_tokens (
    token_hash TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES device_sessions(id) ON DELETE CASCADE,
    used_at INTEGER
);
CREATE INDEX device_refresh_tokens_session ON device_refresh_tokens(session_id);
