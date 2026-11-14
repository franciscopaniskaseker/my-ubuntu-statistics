//! Host/Origin guard against DNS rebinding and cross-site writes.
//!
//! Requests must carry a Host header naming the machine locally
//! (`localhost`, `127.0.0.1`, `[::1]`, the configured listen address or a
//! name from `allowed_hosts`). State-changing requests additionally need a
//! matching `Origin` when the browser sends one.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct HostPolicy {
    allowed: Vec<String>,
    any: bool,
}

impl HostPolicy {
    pub fn new(listen: SocketAddr, extra: &[String]) -> Self {
        let mut allowed: Vec<String> = ["localhost", "127.0.0.1", "::1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ip = listen.ip();
        if !ip.is_unspecified() {
            allowed.push(ip.to_string());
        }
        let any = extra.iter().any(|h| h == "*") || (ip.is_unspecified() && extra.is_empty());
        allowed.extend(extra.iter().map(|h| h.to_ascii_lowercase()));
        Self { allowed, any }
    }

    pub fn allows_any(&self) -> bool {
        self.any
    }

    /// Strip the port and brackets from a Host header value.
    fn hostname(value: &str) -> String {
        let v = value.trim().to_ascii_lowercase();
        if let Some(rest) = v.strip_prefix('[') {
            return rest.split(']').next().unwrap_or("").to_string();
        }
        match v.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') => {
                h.to_string()
            }
            _ => v,
        }
    }

    pub fn host_ok(&self, value: &str) -> bool {
        self.any || self.allowed.contains(&Self::hostname(value))
    }

    pub fn origin_ok(&self, origin: &str) -> bool {
        if self.any {
            return true;
        }
        let rest = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"));
        match rest {
            Some(r) => self.host_ok(r.split('/').next().unwrap_or("")),
            None => false,
        }
    }
}

pub async fn guard(State(policy): State<Arc<HostPolicy>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !policy.host_ok(host) {
        return (StatusCode::MISDIRECTED_REQUEST, "host not allowed").into_response();
    }
    let writes = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if writes
        && let Some(origin) = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        && !policy.origin_ok(origin)
    {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        let p = HostPolicy::new("127.0.0.1:9393".parse().unwrap(), &[]);
        assert!(p.host_ok("127.0.0.1:9393"));
        assert!(p.host_ok("localhost:9393"));
        assert!(p.host_ok("[::1]:9393"));
        assert!(p.host_ok("LOCALHOST"));
        assert!(!p.host_ok("evil.example.com:9393"));
        assert!(!p.host_ok(""));
        assert!(p.origin_ok("http://localhost:9393"));
        assert!(!p.origin_ok("http://evil.example.com"));
        assert!(!p.origin_ok("null"));
        let any = HostPolicy::new("0.0.0.0:9393".parse().unwrap(), &[]);
        assert!(any.host_ok("whatever"));
        let named = HostPolicy::new("0.0.0.0:9393".parse().unwrap(), &["mybox.lan".into()]);
        assert!(named.host_ok("mybox.lan:9393"));
        assert!(!named.host_ok("other"));
    }
}
