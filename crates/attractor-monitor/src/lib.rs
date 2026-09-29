//! PAS Monitor server. Binds 127.0.0.1 only; every request must carry a loopback Host.

mod assets;
pub mod security;

use std::net::{Ipv4Addr, SocketAddr};

use anyhow::Context;
use axum::response::Html;
use axum::routing::get;
use axum::{middleware, Router};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct MonitorOpts {
    pub port: u16,
    pub open: bool,
}

const INDEX: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>PAS Monitor</title>\
<link rel=\"stylesheet\" href=\"/assets/monitor.css\"><script src=\"/assets/htmx.min.js\"></script>\
</head><body><h1>PAS Monitor</h1></body></html>";

pub fn router() -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/assets/:name", get(assets::serve_asset))
        .layer(middleware::from_fn(security::host_guard))
}

/// Bind `127.0.0.1:port`. The address is not configurable.
pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))
}

/// Serve on an existing listener until `shutdown` completes.
pub async fn serve_on(
    listener: TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown)
        .await
        .context("monitor server failed")
}

pub async fn serve(opts: MonitorOpts) -> anyhow::Result<()> {
    let listener = bind(opts.port).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    println!("Monitor listening on {url}");
    if opts.open {
        open_browser(&url);
    }
    serve_on(listener, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    if let Err(e) = cmd.arg(url).spawn() {
        tracing::warn!("could not open browser: {e}");
    }
}
