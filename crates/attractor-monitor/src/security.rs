//! Host and Origin guard. Defends the loopback-only server against DNS rebinding
//! and cross-site requests from other origins.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
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

/// Header carrying the CSRF token on unsafe requests.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Per-process CSRF secret. Regenerated on every Monitor start.
#[derive(Clone)]
pub struct CsrfToken(String);

impl std::fmt::Debug for CsrfToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CsrfToken(..)")
    }
}

impl CsrfToken {
    pub fn generate() -> Self {
        Self(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison against a presented token.
    pub fn matches(&self, presented: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), presented.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

/// True when `origin` is exactly the Monitor's own origin, `http://<host>`.
pub fn is_own_origin(origin: &str, host: &str) -> bool {
    !host.is_empty()
        && origin
            .strip_prefix("http://")
            .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

fn is_unsafe_method(m: &Method) -> bool {
    !matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Middleware: every POST, PUT, PATCH or DELETE needs the CSRF token, and any
/// Origin must be the Monitor's own. Rejects before a handler can run.
pub async fn csrf_guard(
    State(token): State<CsrfToken>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !is_unsafe_method(req.method()) {
        return next.run(req).await;
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !origin.to_str().is_ok_and(|o| is_own_origin(o, host)) {
            return forbidden("forbidden: Origin must be the Monitor's own origin\n");
        }
    }
    let ok = req
        .headers()
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| token.matches(t));
    if !ok {
        return forbidden("forbidden: missing or invalid CSRF token\n");
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_origin_is_exact() {
        assert!(is_own_origin("http://localhost:7777", "localhost:7777"));
        assert!(is_own_origin("http://LocalHost:7777", "localhost:7777"));
        assert!(!is_own_origin("http://localhost:7778", "localhost:7777"));
        assert!(!is_own_origin("http://127.0.0.1:7777", "localhost:7777"));
        assert!(!is_own_origin("https://localhost:7777", "localhost:7777"));
        assert!(!is_own_origin("http://localhost:7777/", "localhost:7777"));
        assert!(!is_own_origin("null", "localhost:7777"));
        assert!(!is_own_origin("http://", ""));
    }

    #[test]
    fn token_matching() {
        let t = CsrfToken::generate();
        assert_eq!(t.as_str().len(), 64);
        assert!(t.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(t.as_str(), CsrfToken::generate().as_str());
        assert!(t.matches(t.as_str()));
        assert!(!t.matches(""));
        assert!(!t.matches(&t.as_str()[..63]));
        assert!(!t.matches(&format!("{}0", t.as_str())));
        let mut flipped = t.as_str().to_string();
        let last = if flipped.ends_with('0') { '1' } else { '0' };
        flipped.pop();
        flipped.push(last);
        assert!(!t.matches(&flipped));
    }

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
