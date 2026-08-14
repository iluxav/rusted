//! Object-storage bindings (`context.objects.<NAME>`): presigned uploads and
//! downloads against S3-compatible endpoints (R2, S3, MinIO), plus host-side
//! head/delete/list.
//!
//! Security shape:
//! - Every key is silently prefixed with an opaque namespace derived from
//!   (owner, function name). The API neither accepts nor returns namespaced
//!   keys, so escaping into another function's objects is structurally
//!   impossible.
//! - Endpoints must be exact origins on the server-admin allowlist
//!   (`RUSTED_OBJECT_ENDPOINTS`); with no allowlist the capability is
//!   disabled. This is what keeps a binding from being an SSRF primitive.
//! - Credentials come from the owner's secret vault, are resolved host-side,
//!   and exist only inside this module's signing calls — never in
//!   `context.env`, stored metadata, responses, or logs.
//! - Signing uses rusty-s3 (SigV4); nothing here implements signing by hand.
//!   PUT signs the exact content length, the SHA-256 checksum, and
//!   `If-None-Match: *`, so the provider itself enforces size, integrity,
//!   and create-only.
//! - Provider and transport errors are redacted before they reach
//!   JavaScript: statuses and shapes, never URLs, query strings, or headers.

use std::time::Duration;

use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::state::now_epoch;

pub const KEY_ENV: &str = "RUSTED_OBJECT_ENDPOINTS";
pub const MAX_KEY_BYTES: usize = 1024;
pub const MIN_EXPIRES_SECONDS: u64 = 15;
pub const MAX_EXPIRES_SECONDS: u64 = 300;
pub const DEFAULT_EXPIRES_SECONDS: u64 = 120;
pub const MAX_LIST_KEYS: usize = 1000;

/// How long the URLs for host-side head/delete/list requests live. Internal:
/// the request is sent immediately.
const INTERNAL_SIGN_SECONDS: u64 = 60;

/// The server side of object bindings: the endpoint allowlist and the HTTP
/// client for host-side operations.
pub struct ObjectHost {
    /// Exact origins bindings may point at, lowercased. Empty = disabled:
    /// pointing functions' credentials at arbitrary hosts is an explicit
    /// server-admin decision, never a default.
    allowlist: Vec<String>,
    /// Local/test mode: permit http:// endpoints (the allowlist still
    /// applies). Production deployments never set this.
    allow_http: bool,
    http: reqwest::Client,
}

impl ObjectHost {
    pub fn from_env() -> ObjectHost {
        let raw = std::env::var(KEY_ENV).unwrap_or_default();
        ObjectHost::with_allowlist(
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| entry.trim_end_matches('/').to_ascii_lowercase())
                .collect(),
            std::env::var("RUSTED_OBJECT_ALLOW_HTTP").ok().as_deref() == Some("1"),
        )
    }

    pub fn with_allowlist(allowlist: Vec<String>, allow_http: bool) -> ObjectHost {
        ObjectHost {
            allowlist,
            allow_http,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client builds"),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.allowlist.is_empty()
    }

    /// Whether a binding may point at `endpoint`. Exact origin comparison —
    /// no prefixes, no subdomain wildcards — so the allowlist means what it
    /// says.
    pub fn allows(&self, endpoint: &str) -> Result<(), String> {
        if self.allowlist.is_empty() {
            return Err(format!(
                "object storage bindings are disabled on this server: set {KEY_ENV} to a \
                 comma-separated list of allowed endpoint origins and restart"
            ));
        }
        let normalized = endpoint.trim_end_matches('/').to_ascii_lowercase();
        if !self.allow_http && normalized.starts_with("http://") {
            return Err("http endpoints are allowed only in local mode; use https".to_string());
        }
        if !self.allowlist.contains(&normalized) {
            return Err(format!(
                "endpoint {endpoint} is not on this server's allowlist ({KEY_ENV})"
            ));
        }
        Ok(())
    }

    /// Performs one operation (JSON in, JSON out) against a binding. The
    /// credentials arrive from the vault and die with this call.
    pub async fn perform(
        &self,
        binding: &rusted_engine::ObjectBinding,
        access_key_id: &str,
        secret_access_key: &str,
        namespace: &str,
        op_json: &str,
    ) -> Result<String, String> {
        // Re-checked per invocation, not only at deploy: an endpoint removed
        // from the allowlist stops being reachable without waiting for a
        // redeploy.
        self.allows(&binding.endpoint)?;
        let op: Op = serde_json::from_str(op_json).map_err(|e| format!("malformed op: {e}"))?;
        let endpoint: reqwest::Url = binding
            .endpoint
            .parse()
            .map_err(|_| "binding endpoint is not a valid URL".to_string())?;
        let bucket = Bucket::new(
            endpoint,
            UrlStyle::Path,
            binding.bucket.clone(),
            binding.region.clone(),
        )
        .map_err(|_| "binding endpoint is not usable as an S3 endpoint".to_string())?;
        let credentials = Credentials::new(access_key_id, secret_access_key);

        let result = match op {
            Op::PresignPut {
                key,
                content_length,
                sha256,
                expires_in_seconds,
            } => {
                let key = namespaced(namespace, &key)?;
                if content_length > binding.max_object_bytes {
                    return Err(format!(
                        "contentLength {content_length} exceeds this binding's maxObjectBytes \
                         ({})",
                        binding.max_object_bytes
                    ));
                }
                let checksum = checksum_base64(&sha256)?;
                let expires = vet_expiry(expires_in_seconds)?;
                let mut action = bucket.put_object(Some(&credentials), &key);
                let length = content_length.to_string();
                // Signed headers: the client must send exactly these, and the
                // provider enforces them — length, integrity, create-only.
                action.headers_mut().insert("content-length", &length);
                action
                    .headers_mut()
                    .insert("x-amz-checksum-sha256", &checksum);
                action.headers_mut().insert("if-none-match", "*");
                let url = action.sign(Duration::from_secs(expires));
                json!({
                    "url": url.as_str(),
                    "headers": {
                        "content-length": length,
                        "x-amz-checksum-sha256": checksum,
                        "if-none-match": "*",
                    },
                    "expiresAt": now_epoch() + expires,
                })
            }
            Op::PresignGet {
                key,
                expires_in_seconds,
            } => {
                let key = namespaced(namespace, &key)?;
                let expires = vet_expiry(expires_in_seconds)?;
                let action = bucket.get_object(Some(&credentials), &key);
                let url = action.sign(Duration::from_secs(expires));
                json!({
                    "url": url.as_str(),
                    "headers": {},
                    "expiresAt": now_epoch() + expires,
                })
            }
            Op::Head { key } => {
                let key = namespaced(namespace, &key)?;
                let mut action = bucket.head_object(Some(&credentials), &key);
                // Without this the provider omits the stored checksum.
                action
                    .headers_mut()
                    .insert("x-amz-checksum-mode", "enabled");
                let url = action.sign(Duration::from_secs(INTERNAL_SIGN_SECONDS));
                let response = self
                    .http
                    .head(url)
                    .header("x-amz-checksum-mode", "enabled")
                    .send()
                    .await
                    .map_err(redact_transport)?;
                match response.status().as_u16() {
                    200 => {
                        let header = |name: &str| {
                            response
                                .headers()
                                .get(name)
                                .and_then(|value| value.to_str().ok())
                                .map(|value| value.to_string())
                        };
                        let content_length = header("content-length")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0);
                        let sha256 =
                            header("x-amz-checksum-sha256").and_then(|b64| checksum_hex(&b64));
                        let last_modified = header("last-modified")
                            .and_then(|raw| httpdate::parse_http_date(&raw).ok())
                            .and_then(|at| {
                                at.duration_since(std::time::UNIX_EPOCH)
                                    .ok()
                                    .map(|d| d.as_secs())
                            });
                        json!({ "head": {
                            "contentLength": content_length,
                            "sha256": sha256,
                            "etag": header("etag").map(|raw| raw.trim_matches('"').to_string()),
                            "lastModified": last_modified,
                        }})
                    }
                    404 => json!({ "head": null }),
                    status => return Err(provider_refused(status)),
                }
            }
            Op::Delete { key } => {
                let key = namespaced(namespace, &key)?;
                let action = bucket.delete_object(Some(&credentials), &key);
                let url = action.sign(Duration::from_secs(INTERNAL_SIGN_SECONDS));
                let response = self
                    .http
                    .delete(url)
                    .send()
                    .await
                    .map_err(redact_transport)?;
                match response.status().as_u16() {
                    200 | 204 => json!({ "deleted": true }),
                    404 => json!({ "deleted": false }),
                    status => return Err(provider_refused(status)),
                }
            }
            Op::List {
                prefix,
                cursor,
                limit,
            } => {
                // The user prefix rides inside the namespace like any key
                // fragment; empty is fine.
                if prefix.len() > MAX_KEY_BYTES || has_forbidden_key_shape(&prefix, true) {
                    return Err("invalid list prefix".to_string());
                }
                let limit = limit.unwrap_or(MAX_LIST_KEYS).clamp(1, MAX_LIST_KEYS);
                let mut action = bucket.list_objects_v2(Some(&credentials));
                action.with_prefix(format!("{namespace}{prefix}"));
                action.with_max_keys(limit);
                if let Some(cursor) = &cursor {
                    action.with_continuation_token(cursor);
                }
                let url = action.sign(Duration::from_secs(INTERNAL_SIGN_SECONDS));
                let response = self.http.get(url).send().await.map_err(redact_transport)?;
                let status = response.status().as_u16();
                if status != 200 {
                    return Err(provider_refused(status));
                }
                let body = response.text().await.map_err(redact_transport)?;
                let parsed = ListObjectsV2::parse_response(&body).map_err(|_| {
                    "the storage provider answered with an unreadable listing".to_string()
                })?;
                let keys: Vec<String> = parsed
                    .contents
                    .iter()
                    .filter_map(|item| item.key.strip_prefix(namespace))
                    .map(|key| key.to_string())
                    .collect();
                match parsed.next_continuation_token {
                    Some(token) => json!({ "keys": keys, "cursor": token }),
                    None => json!({ "keys": keys }),
                }
            }
        };
        Ok(result.to_string())
    }
}

/// The wire ops `context.objects.<NAME>` sends, mirrored by the glue.
#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
enum Op {
    #[serde(rename = "presignPut", rename_all = "camelCase")]
    PresignPut {
        key: String,
        content_length: u64,
        sha256: String,
        #[serde(default)]
        expires_in_seconds: Option<u64>,
    },
    #[serde(rename = "presignGet", rename_all = "camelCase")]
    PresignGet {
        key: String,
        #[serde(default)]
        expires_in_seconds: Option<u64>,
    },
    #[serde(rename = "head")]
    Head { key: String },
    #[serde(rename = "delete")]
    Delete { key: String },
    #[serde(rename = "list")]
    List {
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
}

/// The opaque per-(owner, function, env) prefix every key lives under.
/// Derived, not stored: it survives redeploys and cannot collide across
/// owners. Prod keeps the pre-environment formula so existing objects stay
/// reachable; other envs mix the env into the digest, so a stage invocation
/// physically cannot address prod's objects.
pub fn namespace(user_id: Uuid, function_name: &str, env: &str) -> String {
    let seed = if env == crate::secrets::PROD_ENV {
        format!("{user_id}/{function_name}")
    } else {
        format!("{user_id}/{function_name}@{env}")
    };
    let digest = Sha256::digest(seed.as_bytes());
    format!("{}/", &hex::encode(digest)[..32])
}

/// Validates a caller key and glues it under the namespace.
pub(crate) fn namespaced(namespace: &str, key: &str) -> Result<String, String> {
    if key.is_empty() {
        return Err("object keys cannot be empty".to_string());
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(format!("object keys are at most {MAX_KEY_BYTES} bytes"));
    }
    if has_forbidden_key_shape(key, false) {
        return Err(
            "object keys cannot contain '..', control characters, or start with '/'".to_string(),
        );
    }
    Ok(format!("{namespace}{key}"))
}

fn has_forbidden_key_shape(key: &str, allow_empty: bool) -> bool {
    if key.is_empty() {
        return !allow_empty;
    }
    key.starts_with('/')
        || key.split('/').any(|segment| segment == "..")
        || key.chars().any(|c| c.is_control())
}

pub(crate) fn vet_expiry(requested: Option<u64>) -> Result<u64, String> {
    let expires = requested.unwrap_or(DEFAULT_EXPIRES_SECONDS);
    if !(MIN_EXPIRES_SECONDS..=MAX_EXPIRES_SECONDS).contains(&expires) {
        return Err(format!(
            "expiresInSeconds must be between {MIN_EXPIRES_SECONDS} and {MAX_EXPIRES_SECONDS}"
        ));
    }
    Ok(expires)
}

/// The 64-hex-char SHA-256 a caller supplies, re-encoded the way S3 wants it.
pub(crate) fn checksum_base64(sha256_hex: &str) -> Result<String, String> {
    let bytes = hex::decode(sha256_hex)
        .ok()
        .filter(|b| b.len() == 32 && sha256_hex.chars().all(|c| !c.is_ascii_uppercase()))
        .ok_or_else(|| "sha256 must be 64 lowercase hex characters".to_string())?;
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// A provider checksum header (base64) back to the hex callers speak.
fn checksum_hex(b64: &str) -> Option<String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()
        .filter(|bytes| bytes.len() == 32)
        .map(hex::encode)
}

/// Transport failures carry the URL — and the URL carries the signature — so
/// only the shape of the failure survives.
fn redact_transport(e: reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else {
        "failed"
    };
    format!("storage request {kind}")
}

fn provider_refused(status: u16) -> String {
    format!("the storage provider answered {status}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn binding() -> rusted_engine::ObjectBinding {
        serde_json::from_value(json!({
            "endpoint": "https://acct.r2.cloudflarestorage.com",
            "bucket": "renote-shares",
            "maxObjectBytes": 1024 * 1024,
            "accessKeyIdSecret": "R2_ACCESS_KEY_ID",
            "secretAccessKeySecret": "R2_SECRET_ACCESS_KEY",
        }))
        .unwrap()
    }

    fn host() -> ObjectHost {
        ObjectHost::with_allowlist(vec!["https://acct.r2.cloudflarestorage.com".into()], false)
    }

    #[test]
    fn namespaces_differ_by_owner_function_and_env() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_ne!(namespace(a, "fn", "prod"), namespace(b, "fn", "prod"));
        assert_ne!(namespace(a, "fn", "prod"), namespace(a, "other", "prod"));
        // A stage invocation physically cannot address prod's objects.
        assert_ne!(namespace(a, "fn", "prod"), namespace(a, "fn", "stage"));
        assert_ne!(namespace(a, "fn", "stage"), namespace(a, "fn", "qa"));
        // Stable: a redeploy must land in the same namespace — and prod's
        // formula predates environments, so existing objects stay reachable.
        assert_eq!(namespace(a, "fn", "prod"), namespace(a, "fn", "prod"));
        let legacy = format!(
            "{}/",
            &hex::encode(Sha256::digest(format!("{a}/fn").as_bytes()))[..32]
        );
        assert_eq!(namespace(a, "fn", "prod"), legacy);
        assert!(namespace(a, "fn", "stage").ends_with('/'));
    }

    #[test]
    fn traversal_and_garbage_keys_are_refused() {
        for bad in [
            "",
            "/leading",
            "a/../b",
            "..",
            "../escape",
            "has\ncontrol",
            "has\0nul",
        ] {
            assert!(namespaced("ns/", bad).is_err(), "{bad:?} must be refused");
        }
        assert!(namespaced("ns/", &"x".repeat(1025)).is_err());
        // Dots inside a segment are data, not traversal.
        assert_eq!(namespaced("ns/", "a..b/c.txt").unwrap(), "ns/a..b/c.txt");
    }

    #[tokio::test]
    async fn put_urls_sign_length_checksum_and_create_only() {
        let op = json!({
            "op": "presignPut",
            "key": "folder/doc.bin",
            "contentLength": 42,
            "sha256": "a".repeat(64),
        })
        .to_string();
        let out = host()
            .perform(&binding(), "AKIA", "secret", "ns12345/", &op)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let url = v["url"].as_str().unwrap();
        assert!(url.starts_with(
            "https://acct.r2.cloudflarestorage.com/renote-shares/ns12345/folder/doc.bin?"
        ));
        // The three enforcement headers are signed (the client cannot omit or
        // alter them) and returned for the client to send.
        let signed = url
            .split("X-Amz-SignedHeaders=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .unwrap();
        for header in ["content-length", "if-none-match", "x-amz-checksum-sha256"] {
            assert!(signed.contains(header), "{header} not signed: {signed}");
        }
        assert_eq!(v["headers"]["content-length"], "42");
        assert_eq!(v["headers"]["if-none-match"], "*");
        assert!(v["headers"]["x-amz-checksum-sha256"].is_string());
        assert!(url.contains("X-Amz-Expires=120"));
        assert!(v["expiresAt"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn put_refuses_oversize_bad_checksum_and_bad_expiry() {
        let host = host();
        let b = binding();
        let base = |extra: Value| {
            let mut op = json!({
                "op": "presignPut",
                "key": "k",
                "contentLength": 42,
                "sha256": "a".repeat(64),
            });
            for (k, v) in extra.as_object().unwrap() {
                op[k] = v.clone();
            }
            op.to_string()
        };
        let too_big = base(json!({ "contentLength": 2 * 1024 * 1024 }));
        assert!(host
            .perform(&b, "k", "s", "ns/", &too_big)
            .await
            .unwrap_err()
            .contains("maxObjectBytes"));
        let bad_sum = base(json!({ "sha256": "Z".repeat(64) }));
        assert!(host
            .perform(&b, "k", "s", "ns/", &bad_sum)
            .await
            .unwrap_err()
            .contains("sha256"));
        for expiry in [5, 301] {
            let bad = base(json!({ "expiresInSeconds": expiry }));
            assert!(host
                .perform(&b, "k", "s", "ns/", &bad)
                .await
                .unwrap_err()
                .contains("expiresInSeconds"));
        }
    }

    #[tokio::test]
    async fn endpoints_off_the_allowlist_are_refused() {
        let op = json!({ "op": "presignGet", "key": "k" }).to_string();
        // Disabled entirely.
        let disabled = ObjectHost::with_allowlist(Vec::new(), false);
        let err = disabled
            .perform(&binding(), "k", "s", "ns/", &op)
            .await
            .unwrap_err();
        assert!(err.contains("disabled"), "{err}");
        // Enabled, but for a different origin.
        let other = ObjectHost::with_allowlist(vec!["https://other.example.com".into()], false);
        let err = other
            .perform(&binding(), "k", "s", "ns/", &op)
            .await
            .unwrap_err();
        assert!(err.contains("allowlist"), "{err}");
        // http only in local mode.
        let mut b = binding();
        b.endpoint = "http://127.0.0.1:9000".into();
        let no_http = ObjectHost::with_allowlist(vec!["http://127.0.0.1:9000".into()], false);
        assert!(no_http
            .perform(&b, "k", "s", "ns/", &op)
            .await
            .unwrap_err()
            .contains("local mode"));
        let with_http = ObjectHost::with_allowlist(vec!["http://127.0.0.1:9000".into()], true);
        assert!(with_http.perform(&b, "k", "s", "ns/", &op).await.is_ok());
    }

    #[test]
    fn redaction_keeps_urls_and_credentials_out_of_errors() {
        assert_eq!(provider_refused(403), "the storage provider answered 403");
        // No URL, no query string, no key material in the transport shape.
        for message in [
            provider_refused(500),
            "storage request timed out".to_string(),
        ] {
            assert!(!message.contains("http"), "{message}");
            assert!(!message.contains("X-Amz"), "{message}");
        }
    }
}
