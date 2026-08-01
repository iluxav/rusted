//! Outbound HTTP for handler `fetch()`. Blocking by design: the engine is
//! synchronous and the invocation's wall-clock interrupt still bounds total
//! time. Every request is checked against the plan's per-execution quota and
//! the SSRF guards below.

use std::net::{IpAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct OutboundPolicy {
    /// Requests allowed per invocation (0 disables fetch entirely).
    pub max_requests: u32,
    /// Cap on a single response body.
    pub max_response_bytes: usize,
    /// Per-request timeout; the wall deadline still bounds the whole execution.
    pub timeout: Duration,
}

impl Default for OutboundPolicy {
    fn default() -> Self {
        Self {
            max_requests: 0,
            max_response_bytes: 256 * 1024,
            timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FetchResponse {
    pub ok: bool,
    pub status: u16,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: String,
    /// Set when the request was refused or failed; surfaces as a JS throw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FetchResponse {
    fn refused(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            status: 0,
            headers: Default::default(),
            body: String::new(),
            error: Some(message.into()),
        }
    }
}

/// Per-invocation state: counts requests against the plan quota.
pub struct OutboundBudget {
    policy: OutboundPolicy,
    used: AtomicU32,
    agent: ureq::Agent,
}

impl OutboundBudget {
    pub fn new(policy: OutboundPolicy) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(policy.timeout))
            // Redirects are resolved by us, so each hop gets the same guards.
            .max_redirects(0)
            .build()
            .into();
        Self {
            policy,
            used: AtomicU32::new(0),
            agent,
        }
    }

    pub fn perform(&self, request: FetchRequest) -> FetchResponse {
        if self.policy.max_requests == 0 {
            return FetchResponse::refused(
                "fetch is not available on this plan — upgrade to enable outbound requests",
            );
        }
        let used = self.used.fetch_add(1, Ordering::Relaxed);
        if used >= self.policy.max_requests {
            return FetchResponse::refused(format!(
                "outbound request limit reached ({} per execution on this plan)",
                self.policy.max_requests
            ));
        }
        if let Err(reason) = vet_url(&request.url) {
            return FetchResponse::refused(reason);
        }
        let method = request
            .method
            .unwrap_or_else(|| "GET".into())
            .to_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
        ) {
            return FetchResponse::refused(format!("unsupported method: {method}"));
        }
        // One uniform path: build an http::Request and hand it to the agent,
        // sidestepping ureq's body-typed per-method builders.
        let mut builder = ureq::http::Request::builder()
            .method(method.as_str())
            .uri(&request.url);
        for (k, v) in &request.headers {
            builder = builder.header(k, v);
        }
        let body = request.body.unwrap_or_default();
        let built = match builder.body(body) {
            Ok(built) => built,
            Err(e) => return FetchResponse::refused(format!("bad request: {e}")),
        };
        let result = self.agent.run(built);
        let mut response = match result {
            Ok(response) => response,
            // ureq surfaces non-2xx as errors; unwrap them into real responses.
            Err(ureq::Error::StatusCode(code)) => {
                return FetchResponse {
                    ok: false,
                    status: code,
                    headers: Default::default(),
                    body: String::new(),
                    error: None,
                }
            }
            Err(e) => return FetchResponse::refused(format!("request failed: {e}")),
        };
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response
            .body_mut()
            .with_config()
            .limit(self.policy.max_response_bytes as u64)
            .read_to_string()
            .unwrap_or_default();
        FetchResponse {
            ok: (200..300).contains(&status),
            status,
            headers,
            body,
            error: None,
        }
    }
}

/// http(s) only, and never to loopback/private/link-local addresses — a
/// handler must not be able to reach the host's own network.
fn vet_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("only http and https URLs can be fetched".into());
    }
    let rest = lower.split("://").nth(1).unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => {
            (h, p.parse().unwrap_or(80))
        }
        _ => (authority, if lower.starts_with("https") { 443 } else { 80 }),
    };
    let host = host.trim_matches(['[', ']']);
    if host.is_empty() {
        return Err("the URL has no host".into());
    }
    let addresses: Vec<IpAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .map(|addr| addr.ip())
        .collect();
    if addresses.is_empty() {
        return Err(format!("cannot resolve {host}"));
    }
    if addresses.iter().any(is_private) {
        return Err(format!("{host} resolves to a private address"));
    }
    Ok(())
}

fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                // 100.64.0.0/10 (carrier NAT) and 169.254 are covered above.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique-local and fe80::/10 link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().map(|v4| is_private(&IpAddr::V4(v4))) == Some(true)
        }
    }
}
