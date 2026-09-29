use std::net::SocketAddr;
use std::path::PathBuf;

use attractor_monitor::security::{csrf_guard, CsrfToken};
use attractor_monitor::state::AppState;
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn start(marker: PathBuf, token: CsrfToken) -> SocketAddr {
    let h = move || {
        let m = marker.clone();
        async move {
            std::fs::write(m, "spawned").unwrap();
            "ok"
        }
    };
    let app = Router::new()
        .route("/act", post(h.clone()).put(h))
        .route("/read", get(|| async { "read" }))
        .layer(middleware::from_fn_with_state(token, csrf_guard));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

async fn send(addr: SocketAddr, method: &str, path: &str, headers: &[String]) -> u16 {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: 0\r\n"
    );
    for h in headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf)
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

fn tok(t: &str) -> String {
    format!("X-CSRF-Token: {t}")
}

#[tokio::test]
async fn unsafe_requests_without_valid_token_are_403_and_do_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let token = CsrfToken::generate();
    let addr = start(marker.clone(), token.clone()).await;
    let good = token.as_str().to_string();
    let bad: Vec<Vec<String>> = vec![
        vec![],
        vec![tok("")],
        vec![tok("wrong")],
        vec![tok(&good[..63])],
        vec![tok(&format!("{good}0"))],
        vec![tok(&format!("{}{}", &good[..32], "0".repeat(32)))],
    ];
    for method in ["POST", "PUT"] {
        for h in &bad {
            assert_eq!(send(addr, method, "/act", h).await, 403, "{method} {h:?}");
            assert!(!marker.exists());
        }
    }
    assert_eq!(send(addr, "POST", "/act", &[tok(&good)]).await, 200);
    assert!(marker.exists());
    std::fs::remove_file(&marker).unwrap();
    assert_eq!(send(addr, "PUT", "/act", &[tok(&good)]).await, 200);
    assert!(marker.exists());
}

#[tokio::test]
async fn get_needs_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path().join("m"), CsrfToken::generate()).await;
    assert_eq!(send(addr, "GET", "/read", &[]).await, 200);
}

#[tokio::test]
async fn foreign_origin_is_403_even_with_valid_token() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let token = CsrfToken::generate();
    let addr = start(marker.clone(), token.clone()).await;
    let t = tok(token.as_str());
    let port = addr.port();
    let origins = [
        "http://evil.example".to_string(),
        format!("http://localhost:{port}"), // Host is 127.0.0.1
        format!("http://127.0.0.1:{}", port.wrapping_add(1)),
        "null".to_string(),
        format!("https://127.0.0.1:{port}"),
        format!("http://127.0.0.1:{port}/x"),
    ];
    for o in origins {
        let st = send(addr, "POST", "/act", &[t.clone(), format!("Origin: {o}")]).await;
        assert_eq!(st, 403, "{o}");
        assert!(!marker.exists());
    }
    let own = format!("Origin: http://{addr}");
    assert_eq!(send(addr, "POST", "/act", &[t.clone(), own]).await, 200);
    assert!(marker.exists());
}

#[tokio::test]
async fn real_router_rejects_unknown_post_routes_with_403() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(dir.path().join("index.jsonl"));
    let token = state.csrf_token().clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, attractor_monitor::router(state))
            .await
            .unwrap()
    });
    assert_eq!(send(addr, "POST", "/runs/x/stop", &[]).await, 403);
    assert_eq!(send(addr, "PUT", "/runs/x/stop", &[]).await, 403);
    // With a valid token the guard passes and routing decides (no such route).
    assert_eq!(
        send(addr, "POST", "/runs/x/stop", &[tok(token.as_str())]).await,
        404
    );
}
