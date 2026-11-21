use super::*;
use crate::proxy::ProxySnapshot;
use crate::proxy::tests::{accept_socks, read_header};
use std::time::Duration;

const TOKEN: &str = "test-private-bridge-token";

async fn bridge(network: Arc<ProxyClient>) -> (Bridge, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    (Bridge::start(listener, network, TOKEN.into()), address)
}

async fn authenticated(address: SocketAddr, password: &str) -> (TcpStream, [u8; 2]) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&[5, 1, 2]).await.unwrap();
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 2]);
    let mut auth = vec![1, USERNAME.len() as u8];
    auth.extend_from_slice(USERNAME);
    auth.push(password.len() as u8);
    auth.extend_from_slice(password.as_bytes());
    stream.write_all(&auth).await.unwrap();
    stream.read_exact(&mut reply).await.unwrap();
    (stream, reply)
}

async fn request(address: SocketAddr, destination: SocketAddr) -> (TcpStream, u8) {
    let (mut stream, reply) = authenticated(address, TOKEN).await;
    assert_eq!(reply, [1, 0]);
    let mut request = vec![5, 1, 0];
    match destination.ip() {
        IpAddr::V4(ip) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
    }
    request.extend_from_slice(&destination.port().to_be_bytes());
    stream.write_all(&request).await.unwrap();
    let mut reply = [0; 10];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    (stream, reply[1])
}

#[tokio::test]
async fn supports_authenticated_http_and_socks_upstream_without_losing_bytes() {
    for scheme in ["http", "socks5", "socks5h"] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ProxyConfig::new(
            true,
            &format!(
                "{scheme}://u%40ser:p%3Aass@{}",
                upstream.local_addr().unwrap()
            ),
        )
        .unwrap();
        let network = Arc::new(ProxyClient::new(config).unwrap());
        let (_bridge, address) = bridge(network).await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            if scheme == "http" {
                let header = read_header(&mut stream).await;
                assert!(header.starts_with("CONNECT 203.0.113.8:443 HTTP/1.1\r\n"));
                assert!(header.contains("Proxy-Authorization: Basic dUBzZXI6cDphc3M="));
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\n\r\nready")
                    .await
                    .unwrap();
            } else {
                assert_eq!(
                    accept_socks(&mut stream, true).await,
                    ("203.0.113.8".into(), 443)
                );
                stream.write_all(b"ready").await.unwrap();
            }
            let mut data = [0; 4];
            stream.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });
        let (mut stream, status) = request(address, "203.0.113.8:443".parse().unwrap()).await;
        assert_eq!(status, 0);
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"ready");
        stream.write_all(b"ping").await.unwrap();
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn rejects_missing_or_wrong_internal_credentials() {
    let (_bridge, address) =
        bridge(Arc::new(ProxyClient::new(ProxyConfig::default()).unwrap())).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&[5, 1, 0]).await.unwrap();
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 0xff]);
    let (_, reply) = authenticated(address, "wrong-password").await;
    assert_eq!(reply, [1, 1]);
}

#[tokio::test]
async fn ipv6_targets_are_forwarded_to_http_proxy() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let network = Arc::new(
        ProxyClient::new(
            ProxyConfig::new(true, &format!("http://{}", upstream.local_addr().unwrap())).unwrap(),
        )
        .unwrap(),
    );
    let (_bridge, address) = bridge(network).await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        assert!(
            read_header(&mut stream)
                .await
                .starts_with("CONNECT [2001:db8::1]:443 HTTP/1.1\r\n")
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
    });
    assert_eq!(
        request(address, "[2001:db8::1]:443".parse().unwrap())
            .await
            .1,
        0
    );
    server.await.unwrap();
}

#[tokio::test]
async fn proxy_failure_never_falls_back_and_disabling_closes_existing_connections() {
    let direct = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = direct.local_addr().unwrap();
    let rejected = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config =
        ProxyConfig::new(true, &format!("http://{}", rejected.local_addr().unwrap())).unwrap();
    let network = Arc::new(ProxyClient::new(ProxyConfig::default()).unwrap());
    let (_bridge, address) = bridge(network.clone()).await;
    let (mut old_stream, status) = request(address, destination).await;
    assert_eq!(status, 0);
    let (_remote, _) = direct.accept().await.unwrap();
    // 相同配置不切断连接。
    network.install(ProxySnapshot::new(ProxyConfig::default()).unwrap());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), old_stream.read_u8())
            .await
            .is_err()
    );
    network.install(ProxySnapshot::new(config.clone()).unwrap());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), old_stream.read_u8())
            .await
            .unwrap()
            .is_err()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = rejected.accept().await.unwrap();
        read_header(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            .await
            .unwrap();
    });
    assert_ne!(request(address, destination).await.1, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), direct.accept())
            .await
            .is_err()
    );
    server.await.unwrap();
    // 关闭代理后新连接恢复直连，并保留已保存地址。
    network.install(ProxySnapshot::new(ProxyConfig::new(false, &config.url).unwrap()).unwrap());
    assert_eq!(request(address, destination).await.1, 0);
    direct.accept().await.unwrap();
}

#[test]
fn proxychains_config_is_private_atomic_and_has_no_upstream_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("control/proxychains.conf");
    write_config(&path, 1082, TOKEN).unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    assert!(first.starts_with("strict_chain\n"));
    assert!(first.contains(&format!("socks5 tgdrive 1082 tgdrive {TOKEN}\n")));
    write_config(&path, 1082, "replacement-token").unwrap();
    let second = std::fs::read_to_string(&path).unwrap();
    assert!(!second.contains(TOKEN));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
