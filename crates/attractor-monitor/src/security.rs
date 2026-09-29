//! Host and Origin guard. Defends the loopback-only server against DNS rebinding
//! and cross-site requests from other origins.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// True when `host` (an optional `:port` allowed) names a loopback host.
pub fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, tail)) if tail.is_empty() || is_port(tail.strip_prefix(':')) => inner,
            _ => return false,
        }
    } else {
        match host.split_once(':') {
            Some((name, port)) if is_port(Some(port)) => name,
            Some(_) => return false,
            None => host,
        }
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1"
}

fn is_port(p: Option<&str>) -> bool {
    matches!(p, Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// True when `origin` (`scheme://host[:port]`) has a loopback host.
pub fn is_loopback_origin(origin: &str) -> bool {
    match origin.split_once("://") {
        Some(("http" | "https", authority)) => {
            !authority.contains('/') && is_loopback_host(authority)
        }
        _ => false,
    }
}

fn forbidden(msg: &'static str) -> Response {
    (StatusCode::FORBIDDEN, msg).into_response()
}

/// Middleware: 403 unless Host is loopback and any Origin is loopback.
pub async fn host_guard(req: Request<Body>, next: Next) -> Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_loopback_host);
    if !host_ok {
        return forbidden("forbidden: Host must be a loopback name\n");
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        if !origin.to_str().is_ok_and(is_loopback_origin) {
            return forbidden("forbidden: Origin must be a loopback origin\n");
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_accepted() {
        for h in [
            "localhost",
            "LocalHost:7777",
            "127.0.0.1",
            "127.0.0.1:80",
            "[::1]",
            "[::1]:7777",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn other_hosts_rejected() {
        for h in [
            "",
            "evil.example",
            "evil.example:7777",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "0.0.0.0",
            "::1",
            "[::1",
            "[::1]x",
            "127.0.0.1:",
            "127.0.0.1:abc",
            "localhost:80:80",
            "10.0.0.5",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn origins() {
        assert!(is_loopback_origin("http://localhost:7777"));
        assert!(is_loopback_origin("http://127.0.0.1:1"));
        assert!(!is_loopback_origin("http://evil.example"));
        assert!(!is_loopback_origin("null"));
        assert!(!is_loopback_origin("ftp://localhost"));
        assert!(!is_loopback_origin("http://localhost/evil.example"));
    }
}
