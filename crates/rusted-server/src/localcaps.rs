//! Local-mode adapters for `context.state` and `context.objects` under
//! `rusted run` — no Postgres, no S3, no credentials.
//!
//! State lives in memory: it survives hot reloads (the adapter outlives every
//! reload) and resets when the process exits. Objects live in an isolated
//! temporary directory, fronted by expiring transfer URLs served by the dev
//! server itself — same method, checksum, exact-length, namespace, and
//! create-only behavior as production, so what works locally works deployed.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock, RwLock};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::fnstate;
use crate::objects;
use crate::services::{parse_expected, StateOp};
use crate::state::now_epoch;

/// Local allowances, in the spirit of local limits generally: the most any
/// plan would grant, so nothing blocks mid-thought — while the shape of the
/// rules (key/value sizes, CAS semantics) stays exactly production's.
const LOCAL_MAX_KEYS: usize = 10_000;
const LOCAL_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Local uploads are held in memory during transfer; cap what one PUT may be
/// regardless of the binding's declared ceiling.
pub const LOCAL_MAX_TRANSFER_BYTES: u64 = 256 * 1024 * 1024;

struct StateEntry {
    value: Value,
    version: i64,
    bytes: usize,
}

enum Transfer {
    Put {
        path: PathBuf,
        content_length: u64,
        sha256_hex: String,
        expires_at: u64,
    },
    Get {
        path: PathBuf,
        expires_at: u64,
    },
}

/// The one instance `rusted run` creates at startup and keeps across reloads.
pub struct LocalServices {
    /// Serving function name — the state/object scope. Updated on reload so a
    /// renamed function gets a fresh namespace, exactly as it would deployed.
    function_name: RwLock<String>,
    /// Declared bindings from the current load, for maxObjectBytes.
    bindings: RwLock<BTreeMap<String, rusted_engine::ObjectBinding>>,
    state: Mutex<BTreeMap<String, StateEntry>>,
    objects_root: PathBuf,
    transfers: Mutex<HashMap<String, Transfer>>,
    /// `http://127.0.0.1:<port>`, known only after the listener binds.
    base_url: OnceLock<String>,
}

impl LocalServices {
    pub fn new(function_name: String) -> LocalServices {
        let objects_root = std::env::temp_dir().join(format!(
            "rusted-local-objects-{}-{}",
            std::process::id(),
            crate::auth::random_token(6),
        ));
        LocalServices {
            function_name: RwLock::new(function_name),
            bindings: RwLock::new(BTreeMap::new()),
            state: Mutex::new(BTreeMap::new()),
            objects_root,
            transfers: Mutex::new(HashMap::new()),
            base_url: OnceLock::new(),
        }
    }

    pub fn set_base_url(&self, base: String) {
        let _ = self.base_url.set(base);
    }

    /// Called on every (re)load with the current declarations.
    pub fn reload(&self, function_name: &str, config: &rusted_engine::RuntimeConfig) {
        *self.function_name.write().unwrap() = function_name.to_string();
        *self.bindings.write().unwrap() = config.objects.clone();
    }

    fn namespace_dir(&self) -> PathBuf {
        // Filesystem-safe and collision-free for one process; hashing keeps
        // arbitrary function names out of path semantics.
        let name = self.function_name.read().unwrap();
        let digest = Sha256::digest(name.as_bytes());
        self.objects_root.join(&hex::encode(digest)[..32])
    }

    fn object_path(&self, key: &str) -> Result<PathBuf, String> {
        // Same validation as production, then the key maps into the namespace
        // directory. Traversal is rejected before any path is built.
        objects::namespaced("", key)?;
        Ok(self.namespace_dir().join(key))
    }

    async fn state_op(&self, op_json: String) -> Result<String, String> {
        let op: StateOp =
            serde_json::from_str(&op_json).map_err(|e| format!("malformed op: {e}"))?;
        let mut state = self.state.lock().unwrap();
        let result = match op {
            StateOp::Get { key } => {
                vet_key(&key)?;
                match state.get(&key) {
                    Some(entry) => {
                        json!({ "entry": { "key": key, "value": entry.value, "version": entry.version } })
                    }
                    None => json!({ "entry": null }),
                }
            }
            StateOp::Cas {
                key,
                expected_version,
                value,
            } => {
                vet_key(&key)?;
                let expected = parse_expected(&expected_version)?;
                let serialized = serde_json::to_string(&value).map_err(|e| e.to_string())?;
                if serialized.len() > fnstate::MAX_VALUE_BYTES {
                    return Err(format!(
                        "state value is {} bytes serialized; the limit is {}",
                        serialized.len(),
                        fnstate::MAX_VALUE_BYTES
                    ));
                }
                let current = state.get(&key).map(|entry| entry.version);
                match (expected, current) {
                    (None, None) => {
                        if state.len() >= LOCAL_MAX_KEYS {
                            return Err(format!("state limit: {LOCAL_MAX_KEYS} keys"));
                        }
                        let total: usize = state.values().map(|entry| entry.bytes).sum();
                        if total + serialized.len() > LOCAL_MAX_BYTES {
                            return Err(format!("state limit: {LOCAL_MAX_BYTES} bytes"));
                        }
                        state.insert(
                            key,
                            StateEntry {
                                value,
                                version: 1,
                                bytes: serialized.len(),
                            },
                        );
                        json!({ "ok": true, "version": 1 })
                    }
                    (Some(expected), Some(current)) if expected == current => {
                        let entry = state.get_mut(&key).expect("checked above");
                        entry.value = value;
                        entry.version += 1;
                        entry.bytes = serialized.len();
                        json!({ "ok": true, "version": entry.version })
                    }
                    (_, current) => json!({ "ok": false, "currentVersion": current }),
                }
            }
            StateOp::Delete {
                key,
                expected_version,
            } => {
                vet_key(&key)?;
                let expected = parse_expected(&expected_version)?
                    .ok_or_else(|| "delete needs the expected version".to_string())?;
                let current = state.get(&key).map(|entry| entry.version);
                if current == Some(expected) {
                    state.remove(&key);
                    json!({ "ok": true })
                } else {
                    json!({ "ok": false, "currentVersion": current })
                }
            }
            StateOp::List {
                prefix,
                cursor,
                limit,
            } => {
                let limit = limit.unwrap_or(100).clamp(1, fnstate::MAX_LIST_LIMIT) as usize;
                let mut items: Vec<Value> = state
                    .iter()
                    .filter(|(key, _)| key.starts_with(&prefix) && key.as_str() > cursor.as_str())
                    .take(limit + 1)
                    .map(|(key, entry)| {
                        json!({ "key": key, "value": entry.value, "version": entry.version })
                    })
                    .collect();
                if items.len() > limit {
                    items.truncate(limit);
                    let last = items
                        .last()
                        .and_then(|item| item["key"].as_str())
                        .map(String::from);
                    json!({ "items": items, "cursor": last })
                } else {
                    json!({ "items": items })
                }
            }
        };
        Ok(result.to_string())
    }

    async fn object_op(&self, binding_name: String, op_json: String) -> Result<String, String> {
        let max_object_bytes = {
            let bindings = self.bindings.read().unwrap();
            let Some(binding) = bindings.get(&binding_name) else {
                return Err(format!("no object binding named {binding_name}"));
            };
            binding.max_object_bytes
        };
        #[derive(serde::Deserialize)]
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
        let op: Op = serde_json::from_str(&op_json).map_err(|e| format!("malformed op: {e}"))?;
        let base = self
            .base_url
            .get()
            .cloned()
            .ok_or_else(|| "the dev server is still starting".to_string())?;

        let result = match op {
            Op::PresignPut {
                key,
                content_length,
                sha256,
                expires_in_seconds,
            } => {
                let path = self.object_path(&key)?;
                if content_length > max_object_bytes {
                    return Err(format!(
                        "contentLength {content_length} exceeds this binding's maxObjectBytes \
                         ({max_object_bytes})"
                    ));
                }
                if content_length > LOCAL_MAX_TRANSFER_BYTES {
                    return Err(format!(
                        "local mode caps transfers at {LOCAL_MAX_TRANSFER_BYTES} bytes"
                    ));
                }
                let checksum = objects::checksum_base64(&sha256)?;
                let expires = objects::vet_expiry(expires_in_seconds)?;
                let token = crate::auth::random_token(24);
                let expires_at = now_epoch() + expires;
                self.prune_transfers();
                self.transfers.lock().unwrap().insert(
                    token.clone(),
                    Transfer::Put {
                        path,
                        content_length,
                        sha256_hex: sha256,
                        expires_at,
                    },
                );
                json!({
                    "url": format!("{base}/_objects/{token}"),
                    "headers": {
                        "content-length": content_length.to_string(),
                        "x-amz-checksum-sha256": checksum,
                        "if-none-match": "*",
                    },
                    "expiresAt": expires_at,
                })
            }
            Op::PresignGet {
                key,
                expires_in_seconds,
            } => {
                let path = self.object_path(&key)?;
                let expires = objects::vet_expiry(expires_in_seconds)?;
                let token = crate::auth::random_token(24);
                let expires_at = now_epoch() + expires;
                self.prune_transfers();
                self.transfers
                    .lock()
                    .unwrap()
                    .insert(token.clone(), Transfer::Get { path, expires_at });
                json!({
                    "url": format!("{base}/_objects/{token}"),
                    "headers": {},
                    "expiresAt": expires_at,
                })
            }
            Op::Head { key } => {
                let path = self.object_path(&key)?;
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let last_modified = std::fs::metadata(&path)
                            .ok()
                            .and_then(|meta| meta.modified().ok())
                            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs());
                        json!({ "head": {
                            "contentLength": bytes.len(),
                            "sha256": hex::encode(Sha256::digest(&bytes)),
                            "etag": Value::Null,
                            "lastModified": last_modified,
                        }})
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({ "head": null }),
                    Err(_) => return Err("could not read the local object".to_string()),
                }
            }
            Op::Delete { key } => {
                let path = self.object_path(&key)?;
                match std::fs::remove_file(&path) {
                    Ok(()) => json!({ "deleted": true }),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        json!({ "deleted": false })
                    }
                    Err(_) => return Err("could not delete the local object".to_string()),
                }
            }
            Op::List {
                prefix,
                cursor,
                limit,
            } => {
                let limit = limit
                    .unwrap_or(objects::MAX_LIST_KEYS)
                    .clamp(1, objects::MAX_LIST_KEYS);
                let root = self.namespace_dir();
                let mut keys = Vec::new();
                collect_keys(&root, &root, &mut keys);
                keys.sort();
                let cursor = cursor.unwrap_or_default();
                let mut page: Vec<String> = keys
                    .into_iter()
                    .filter(|key| key.starts_with(&prefix) && key.as_str() > cursor.as_str())
                    .take(limit + 1)
                    .collect();
                if page.len() > limit {
                    page.truncate(limit);
                    let last = page.last().cloned();
                    json!({ "keys": page, "cursor": last })
                } else {
                    json!({ "keys": page })
                }
            }
        };
        Ok(result.to_string())
    }

    fn prune_transfers(&self) {
        let now = now_epoch();
        self.transfers.lock().unwrap().retain(|_, transfer| {
            let expires_at = match transfer {
                Transfer::Put { expires_at, .. } => *expires_at,
                Transfer::Get { expires_at, .. } => *expires_at,
            };
            expires_at > now
        });
    }

    /// One PUT against a transfer URL: exact length, exact checksum,
    /// create-only — the same three the provider enforces in production.
    /// `(status, message)` mirrors what an S3 endpoint would answer.
    pub fn accept_put(&self, token: &str, body: &[u8]) -> (u16, String) {
        let transfers = self.transfers.lock().unwrap();
        let Some(Transfer::Put {
            path,
            content_length,
            sha256_hex,
            expires_at,
        }) = transfers.get(token)
        else {
            return (403, "unknown or non-PUT transfer".into());
        };
        if *expires_at <= now_epoch() {
            return (403, "this upload URL has expired".into());
        }
        if body.len() as u64 != *content_length {
            return (
                400,
                "body length differs from the signed contentLength".into(),
            );
        }
        if hex::encode(Sha256::digest(body)) != *sha256_hex {
            return (400, "body checksum differs from the signed sha256".into());
        }
        if path.exists() {
            return (
                412,
                "the key already exists (uploads are create-only)".into(),
            );
        }
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return (500, "could not create the local object directory".into());
            }
        }
        match std::fs::write(path, body) {
            Ok(()) => (200, "stored".into()),
            Err(_) => (500, "could not write the local object".into()),
        }
    }

    /// One GET against a transfer URL.
    pub fn accept_get(&self, token: &str) -> Result<Vec<u8>, (u16, String)> {
        let transfers = self.transfers.lock().unwrap();
        let Some(Transfer::Get { path, expires_at }) = transfers.get(token) else {
            return Err((403, "unknown or non-GET transfer".into()));
        };
        if *expires_at <= now_epoch() {
            return Err((403, "this download URL has expired".into()));
        }
        std::fs::read(path).map_err(|_| (404, "no such object".into()))
    }
}

fn vet_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > fnstate::MAX_KEY_BYTES {
        return Err(format!(
            "state keys are 1-{} bytes of UTF-8",
            fnstate::MAX_KEY_BYTES
        ));
    }
    Ok(())
}

fn collect_keys(root: &std::path::Path, dir: &std::path::Path, keys: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_keys(root, &path, keys);
        } else if let Ok(relative) = path.strip_prefix(root) {
            keys.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

impl rusted_engine::HostServices for LocalServices {
    fn inbox_get(
        &self,
        _name: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Err("local mode has no inboxes".to_string()) })
    }

    /// Keeps `context.inbox` undefined locally, exactly as before these
    /// adapters existed — an absent capability, not a broken one.
    fn offers_inbox(&self) -> bool {
        false
    }

    fn state_op(
        &self,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(self.state_op(op_json))
    }

    fn object_op(
        &self,
        binding: String,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(self.object_op(binding, op_json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn services() -> LocalServices {
        let s = LocalServices::new("my-fn".into());
        s.set_base_url("http://127.0.0.1:7400".into());
        let config: rusted_engine::RuntimeConfig = serde_json::from_value(json!({
            "state": true,
            "objects": { "FILES": {
                "endpoint": "https://ignored.example.com",
                "bucket": "ignored-bkt",
                "maxObjectBytes": 1024,
                "accessKeyIdSecret": "AK",
                "secretAccessKeySecret": "SK",
            }},
        }))
        .unwrap();
        s.reload("my-fn", &config);
        s
    }

    async fn state(s: &LocalServices, op: Value) -> Value {
        serde_json::from_str(&s.state_op(op.to_string()).await.unwrap()).unwrap()
    }

    async fn object(s: &LocalServices, op: Value) -> Result<Value, String> {
        s.object_op("FILES".into(), op.to_string())
            .await
            .map(|raw| serde_json::from_str(&raw).unwrap())
    }

    /// The exact sequence Renote needs: CAS create → conflict → update, then
    /// list — production semantics, in memory.
    #[tokio::test]
    async fn local_state_speaks_production_cas() {
        let s = services();
        let created = state(
            &s,
            json!({"op":"cas","key":"k","expectedVersion":null,"value":{"n":1}}),
        )
        .await;
        assert_eq!(created, json!({"ok": true, "version": 1}));
        let conflicted = state(
            &s,
            json!({"op":"cas","key":"k","expectedVersion":null,"value":2}),
        )
        .await;
        assert_eq!(conflicted, json!({"ok": false, "currentVersion": 1}));
        let updated = state(
            &s,
            json!({"op":"cas","key":"k","expectedVersion":1,"value":{"n":2}}),
        )
        .await;
        assert_eq!(updated, json!({"ok": true, "version": 2}));
        let got = state(&s, json!({"op":"get","key":"k"})).await;
        assert_eq!(got["entry"]["value"], json!({"n": 2}));
        let listed = state(&s, json!({"op":"list","prefix":"","cursor":"","limit":10})).await;
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        let deleted = state(&s, json!({"op":"delete","key":"k","expectedVersion":2})).await;
        assert_eq!(deleted, json!({"ok": true}));
    }

    /// presignPut → PUT (checksum, length, create-only) → presignGet → GET —
    /// the whole transfer loop the dev server serves.
    #[tokio::test]
    async fn local_objects_enforce_the_upload_contract() {
        let s = services();
        let body = b"hello";
        let sha = hex::encode(Sha256::digest(body));
        let put = object(
            &s,
            json!({
                "op": "presignPut", "key": "docs/a.bin",
                "contentLength": 5, "sha256": sha,
            }),
        )
        .await
        .unwrap();
        let token = put["url"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(put["headers"]["content-length"], "5");
        assert_eq!(put["headers"]["if-none-match"], "*");

        // Wrong length, wrong bytes: refused, nothing stored.
        assert_eq!(s.accept_put(&token, b"hello!").0, 400);
        assert_eq!(s.accept_put(&token, b"HELLO").0, 400);
        assert_eq!(
            object(&s, json!({"op":"head","key":"docs/a.bin"}))
                .await
                .unwrap()["head"],
            Value::Null
        );

        // The exact bytes: stored; a second identical PUT: create-only 412.
        assert_eq!(s.accept_put(&token, body).0, 200);
        assert_eq!(s.accept_put(&token, body).0, 412);

        let head = object(&s, json!({"op":"head","key":"docs/a.bin"}))
            .await
            .unwrap();
        assert_eq!(head["head"]["contentLength"], 5);
        assert_eq!(head["head"]["sha256"].as_str().unwrap(), sha);

        let get = object(&s, json!({"op":"presignGet","key":"docs/a.bin"}))
            .await
            .unwrap();
        let get_token = get["url"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(s.accept_get(&get_token).unwrap(), body);
        // A PUT token cannot download and vice versa.
        assert!(s.accept_get(&token).is_err());

        let listed = object(&s, json!({"op":"list"})).await.unwrap();
        assert_eq!(listed["keys"], json!(["docs/a.bin"]));
        let deleted = object(&s, json!({"op":"delete","key":"docs/a.bin"}))
            .await
            .unwrap();
        assert_eq!(deleted["deleted"], true);
    }

    #[tokio::test]
    async fn local_objects_share_production_validation() {
        let s = services();
        // Traversal and size rules are the same code as production.
        for bad in ["../escape", "/lead", "a/../b"] {
            let err = object(&s, json!({"op":"head","key":bad}))
                .await
                .unwrap_err();
            assert!(err.contains("keys cannot"), "{bad}: {err}");
        }
        // maxObjectBytes comes from the declared binding.
        let err = object(
            &s,
            json!({
                "op": "presignPut", "key": "big",
                "contentLength": 2048, "sha256": "a".repeat(64),
            }),
        )
        .await
        .unwrap_err();
        assert!(err.contains("maxObjectBytes"), "{err}");
        // Expired transfers refuse.
        let put = object(
            &s,
            json!({
                "op": "presignPut", "key": "late",
                "contentLength": 1, "sha256": "a".repeat(64), "expiresInSeconds": 15,
            }),
        )
        .await
        .unwrap();
        let token = put["url"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        if let Some(Transfer::Put { expires_at, .. }) = s.transfers.lock().unwrap().get_mut(&token)
        {
            *expires_at = 0;
        }
        assert_eq!(s.accept_put(&token, b"x").0, 403);
    }
}
