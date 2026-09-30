use super::*;

#[test]
fn default_transport_uses_http2_and_quic_remains_explicitly_available() {
    assert_eq!(Transport::default(), Transport::Http2);
    for (value, http2) in [("http2", true), ("quic", false)] {
        let transport = Transport::parse(value).unwrap();
        let mut cmd = command(Path::new("usque"), Path::new("config.json"));
        cmd.arg("socks");
        transport.configure(&mut cmd);
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert_eq!(args.iter().any(|arg| *arg == "--http2"), http2);
        assert!(!args.iter().any(|arg| *arg == "--insecure"));
    }
    assert!(Transport::parse("invalid").is_err());
}

fn fixture(path: &Path) {
    std::fs::write(path, r#"{"private_key":"test","endpoint_pub_key":"test","endpoint_v4":"192.0.2.1","ipv4":"100.64.0.1","ipv6":"::1"}"#).unwrap();
}

#[test]
fn tunnel_status_distinguishes_listener_connection_and_reconnect_without_raw_logs() {
    assert!(tunnel_event("SOCKS proxy listening on 127.0.0.1:1083").is_none());
    assert_eq!(
        tunnel_event("Connected to MASQUE server").unwrap().0,
        "running"
    );
    let (phase, message) = tunnel_event("Tunnel connection lost: private-secret").unwrap();
    assert_eq!(phase, "starting");
    assert!(!message.contains("private-secret"));
    assert_eq!(
        tunnel_event("Failed to connect tunnel: timeout").unwrap().0,
        "starting"
    );
}

#[tokio::test]
async fn registration_requires_explicit_consent() {
    let error = register(false).await.err().unwrap().to_string();
    assert!(error.contains("条款"));
}

#[tokio::test]
async fn unregistered_and_missing_binary_fail_without_exposing_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let (tx, _) = watch::channel(status("starting", "", false));
    assert!(
        run(Path::new("/missing-usque"), &config, 1083, "secret", &tx)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("尚未初始化")
    );
    fixture(&config);
    assert!(valid_config(&config));
    let error = run(Path::new("/missing-usque"), &config, 1083, "secret", &tx)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("无法启动"));
    assert!(!error.contains("secret"));
    std::fs::write(&config, "{}").unwrap();
    assert!(!valid_config(&config));
}

#[cfg(unix)]
#[tokio::test]
async fn supervised_process_restarts_and_stops_after_last_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    fixture(&config);
    let executable = dir.path().join("usque");
    // shell 仅用于测试子进程；生产始终直接执行固定的 usque 二进制。
    std::fs::write(
        &executable,
        "#!/bin/sh\necho $$ >> \"${2}.pids\"\nsleep 0.3\necho 'Connected to MASQUE server' >&2\nexec sleep 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 2]);
            stream.write_all(&[5, 2]).await.unwrap();
            let mut auth = [0; 16];
            stream.read_exact(&mut auth).await.unwrap();
            assert_eq!(&auth[2..9], b"tgdrive");
            assert_eq!(&auth[10..], b"secret");
            stream.write_all(&[1, 0]).await.unwrap();
        }
    });
    let service = Service::start(executable, config.clone(), port, "secret".into());
    // SOCKS 已监听仍须等待隧道就绪事件，不能把本地端口等同于联网成功。
    assert!(
        tokio::time::timeout(Duration::from_millis(100), service.ready())
            .await
            .is_err()
    );
    service.ready().await.unwrap();
    let pids = config.with_extension("json.pids");
    tokio::time::timeout(Duration::from_secs(9), async {
        loop {
            if std::fs::read_to_string(&pids)
                .unwrap_or_default()
                .lines()
                .count()
                >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let owner = service.clone();
    drop(service);
    assert!(!owner.task.is_finished());
    let task = owner.task.abort_handle();
    drop(owner);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    #[cfg(target_os = "linux")]
    {
        let last_pid = std::fs::read_to_string(&pids)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .to_string();
        tokio::time::timeout(Duration::from_secs(2), async {
            while Path::new(&format!("/proc/{last_pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
    server.abort();
}
