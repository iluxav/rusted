//! Outbound HTTP for handler `fetch()`. Blocking by design, which has a
//! consequence worth stating plainly: while a fetch is in flight no JavaScript
//! runs, so QuickJS's interrupt handler cannot fire and the wall deadline
//! cannot stop it. A 500ms budget did not interrupt a 10s fetch, and three
//! two-second fetches took six seconds.
//!
//! So the deadline is enforced here instead. Each request gets whatever time
//! the invocation has left, and one past the deadline is refused before any
//! connection work. That keeps `exec_ms` an honest bound on total wall time
//! rather than a bound on JavaScript alone.

use std::net::{IpAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct OutboundPolicy {
    /// Requests allowed per invocation (0 disables fetch entirely).
    pub max_requests: u32,
    /// Cap on a single response body.
    pub max_response_bytes: usize,
    /// Ceiling for one request. The invocation's remaining time can lower it,
    /// never raise it.
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

/// Per-invocation state: counts requests against the plan quota and holds the
/// deadline the whole invocation shares.
pub struct OutboundBudget {
    policy: OutboundPolicy,
    used: AtomicU32,
    /// When this invocation must be finished. `None` outside an invocation —
    /// `verify` and local dev, where nothing is holding a worker slot.
    deadline: Option<Instant>,
}

impl OutboundBudget {
    pub fn new(policy: OutboundPolicy) -> Self {
        Self {
            policy,
            used: AtomicU32::new(0),
            deadline: None,
        }
    }

    /// The usual case: fetches share the invocation's wall-clock budget.
    pub fn with_deadline(policy: OutboundPolicy, deadline: Instant) -> Self {
        Self {
            policy,
            used: AtomicU32::new(0),
            deadline: Some(deadline),
        }
    }

    /// How long the next request may take: the policy ceiling, or the time the
    /// invocation has left, whichever is smaller. `None` once that is gone.
    fn effective_timeout(&self) -> Option<Duration> {
        match self.deadline {
            None => Some(self.policy.timeout),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                (!left.is_zero()).then(|| self.policy.timeout.min(left))
            }
        }
    }

    /// A fresh agent per request, because the timeout is per request and ureq
    /// fixes it at construction. Costs connection reuse between fetches in one
    /// invocation; buys a bound that actually holds.
    fn agent(&self, timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Redirects are resolved by us, so each hop gets the same guards.
            .max_redirects(0)
            .build()
            .into()
    }

    /// Requests actually attempted during this invocation.
    pub fn used(&self) -> u32 {
        self.used.load(Ordering::Relaxed)
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
        // Before DNS or connect: past the deadline, nothing may start.
        let Some(timeout) = self.effective_timeout() else {
            return FetchResponse::refused(
                "execution deadline reached before this fetch could start",
            );
        };
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
        let result = self.agent(timeout).run(built);
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

#[cfg(test)]
mod deadline_tests {
    use super::*;

    fn policy() -> OutboundPolicy {
        OutboundPolicy {
            max_requests: 10,
            max_response_bytes: 1024,
            timeout: Duration::from_secs(30),
        }
    }

    /// Past the invocation's deadline, a fetch must be refused before any DNS
    /// or connection work — otherwise it extends a slot hold that is already
    /// over budget.
    #[test]
    fn a_fetch_after_the_deadline_is_refused_immediately() {
        let past = std::time::Instant::now() - Duration::from_millis(10);
        let budget = OutboundBudget::with_deadline(policy(), past);
        let started = std::time::Instant::now();
        let r = budget.perform(FetchRequest {
            url: "http://198.51.100.1/never".into(),
            method: None,
            headers: Default::default(),
            body: None,
        });
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "it tried to connect"
        );
        assert!(
            r.error.as_deref().unwrap_or("").contains("deadline"),
            "should say the deadline is why: {:?}",
            r.error
        );
    }

    /// Within the deadline, a fetch may not be given longer than the time the
    /// invocation has left, however generous the policy timeout is.
    #[test]
    fn a_fetch_is_capped_by_the_time_the_invocation_has_left() {
        let soon = std::time::Instant::now() + Duration::from_millis(200);
        let budget = OutboundBudget::with_deadline(policy(), soon);
        let effective = budget
            .effective_timeout()
            .expect("still inside the deadline");
        assert!(
            effective <= Duration::from_millis(200),
            "capped to the remaining time, got {effective:?}"
        );
        assert!(
            effective < policy().timeout,
            "the 30s policy timeout must not win"
        );
    }

    /// With no deadline (verify, local dev), the policy timeout stands.
    #[test]
    fn without_a_deadline_the_policy_timeout_stands() {
        let budget = OutboundBudget::new(policy());
        assert_eq!(budget.effective_timeout(), Some(Duration::from_secs(30)));
    }
}
