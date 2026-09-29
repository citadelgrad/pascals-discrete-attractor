//! Vendored static assets, embedded so they serve with no network and no working-directory lookup.

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

pub const HTMX: &[u8] = include_bytes!("../assets/htmx.min.js");
pub const HTMX_SSE: &[u8] = include_bytes!("../assets/htmx-sse.js");
pub const VIZ: &[u8] = include_bytes!("../assets/viz-standalone.js");
pub const CSS_BYTES: &[u8] = include_bytes!("../assets/monitor.css");

/// Exact-name lookup, so no path can escape the asset set.
fn lookup(name: &str) -> Option<(&'static str, &'static [u8])> {
    match name {
        "htmx.min.js" => Some((JS, HTMX)),
        "htmx-sse.js" => Some((JS, HTMX_SSE)),
        "viz-standalone.js" => Some((JS, VIZ)),
        "monitor.css" => Some((CSS, CSS_BYTES)),
        _ => None,
    }
}

pub async fn serve_asset(Path(name): Path<String>) -> Response {
    match lookup(&name) {
        Some((ct, body)) => ([(header::CONTENT_TYPE, ct)], body).into_response(),
        None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}
