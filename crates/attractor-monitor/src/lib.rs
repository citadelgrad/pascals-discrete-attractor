//! PAS Monitor server. Binds 127.0.0.1 only; every request must carry a loopback Host.

mod assets;
pub mod controls;
pub mod findings;
pub mod projection;
pub mod security;
pub mod spawn;
pub mod sse;
pub mod state;
pub mod views;
pub mod watcher;

use std::net::{Ipv4Addr, SocketAddr};

use anyhow::Context;
use axum::routing::{get, post};
use axum::{middleware, Router};
use state::AppState;
use tokio::net::TcpListener;

/// How often the watcher re-reads the Run Index.
const INDEX_POLL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct MonitorOpts {
    pub port: u16,
    pub open: bool,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/runs/:id/events", get(sse::run_events))
        .route("/", get(views::runs::page_handler))
        .route("/runs", get(views::runs::table_handler))
        .route("/runs/:id", get(views::run::page_handler))
        .route("/runs/:id/summary", get(views::run::summary_handler))
        .route(
            "/runs/:id/transcripts/:inv",
            get(views::transcript::handler),
        )
        .route("/runs/:id/stop", post(controls::stop))
        .route("/runs/:id/kill", post(controls::kill))
        .route("/runs/:id/resume", post(controls::resume))
        .route("/runs/:id/rerun", post(controls::rerun))
        .route("/assets/:name", get(assets::serve_asset))
        .layer(middleware::from_fn_with_state(
            state.csrf_token().clone(),
            security::csrf_guard,
        ))
        .layer(middleware::from_fn(security::host_guard))
        .with_state(state)
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
    let index = attractor_journal::index_path().context("cannot resolve the Run Index path")?;
    let state = AppState::new(index.clone());
    let watcher = watcher::spawn(state.clone(), index, INDEX_POLL);
    let result = serve_on_with(listener, state, shutdown).await;
    watcher.abort();
    result
}

/// Serve on an existing listener with a caller-managed [`AppState`].
pub async fn serve_on_with(
    listener: TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    axum::serve(listener, router(state))
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
