use std::net::{IpAddr, SocketAddr, UdpSocket};

use attractor_monitor::{bind, serve, serve_on, MonitorOpts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn start() -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let l = bind(0).await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(serve_on(l, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

/// Raw request; returns (status, headers+body text).
async fn raw(addr: SocketAddr, path: &str, headers: &[&str]) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nConnection: close\r\n");
    for h in headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    (status, text)
}

#[tokio::test]
async fn serve_reports_bound_loopback_address() {
    let l = bind(0).await.unwrap();
    let a = l.local_addr().unwrap();
    assert_eq!(a.ip(), IpAddr::from([127, 0, 0, 1]));
}

#[tokio::test]
async fn listens_on_requested_port() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let l = bind(port).await.unwrap();
    assert_eq!(l.local_addr().unwrap().port(), port);
}

#[tokio::test]
async fn non_loopback_address_is_refused() {
    let ip = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip());
    let (addr, _stop) = start().await;
    match ip {
        Ok(ip) if !ip.is_loopback() && !ip.is_unspecified() => {
            let err = TcpStream::connect((ip, addr.port())).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
        }
        other => eprintln!("SKIPPED: no non-loopback interface ({other:?})"),
    }
}

#[tokio::test]
async fn foreign_host_is_403() {
    let (addr, _stop) = start().await;
    let p = addr.port();
    for h in [
        "Host: evil.example".to_string(),
        format!("Host: evil.example:{p}"),
        "Host: 127.0.0.1.evil.example".to_string(),
    ] {
        assert_eq!(raw(addr, "/", &[&h]).await.0, 403, "{h}");
        assert_eq!(raw(addr, "/assets/htmx.min.js", &[&h]).await.0, 403, "{h}");
    }
    // HTTP/1.1 without Host is rejected by hyper (400) or by the guard (403); never served.
    let (status, _) = raw(addr, "/", &[]).await;
    assert!(status == 400 || status == 403, "{status}");
}

#[tokio::test]
async fn loopback_hosts_are_served() {
    let (addr, _stop) = start().await;
    let p = addr.port();
    for h in [
        "localhost".to_string(),
        format!("localhost:{p}"),
        format!("127.0.0.1:{p}"),
        format!("[::1]:{p}"),
    ] {
        assert_eq!(raw(addr, "/", &[&format!("Host: {h}")]).await.0, 200, "{h}");
    }
}

#[tokio::test]
async fn foreign_origin_is_403() {
    let (addr, _stop) = start().await;
    let p = addr.port();
    let host = format!("Host: 127.0.0.1:{p}");
    assert_eq!(
        raw(addr, "/", &[&host, "Origin: http://evil.example"])
            .await
            .0,
        403
    );
    assert_eq!(raw(addr, "/", &[&host, "Origin: null"]).await.0, 403);
    assert_eq!(
        raw(
            addr,
            "/",
            &[&host, &format!("Origin: http://localhost:{p}")]
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn assets_are_served_offline() {
    let (addr, _stop) = start().await;
    let host = format!("Host: 127.0.0.1:{}", addr.port());
    let (status, text) = raw(addr, "/assets/htmx.min.js", &[&host]).await;
    assert_eq!(status, 200);
    assert!(text
        .to_ascii_lowercase()
        .contains("content-type: text/javascript"));
    assert!(text.contains("htmx"));
    for (name, ct) in [
        ("htmx-sse.js", "text/javascript"),
        ("viz-standalone.js", "text/javascript"),
        ("monitor.css", "text/css"),
    ] {
        let (status, text) = raw(addr, &format!("/assets/{name}"), &[&host]).await;
        assert_eq!(status, 200, "{name}");
        assert!(
            text.to_ascii_lowercase()
                .contains(&format!("content-type: {ct}")),
            "{name}"
        );
    }
}

#[tokio::test]
async fn unknown_and_traversal_assets_are_404() {
    let (addr, _stop) = start().await;
    let host = format!("Host: 127.0.0.1:{}", addr.port());
    for p in [
        "/assets/nope.js",
        "/assets/..%2fCargo.toml",
        "/assets/%2e%2e/Cargo.toml",
        "/assets/../Cargo.toml",
        "/assets/",
    ] {
        assert_eq!(raw(addr, p, &[&host]).await.0, 404, "{p}");
    }
}

#[tokio::test]
async fn port_in_use_error_names_port() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let err = serve(MonitorOpts { port, open: false }).await.unwrap_err();
    assert!(format!("{err:#}").contains(&port.to_string()), "{err:#}");
}
