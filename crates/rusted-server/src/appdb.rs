//! The account database behind `context.db`: one SQLite file per
//! (owner, env), opened inside this process, serving queries in microseconds.
//!
//! The load-bearing property: Postgres is never in the query path. The local
//! file (plus SQLite's page cache) is the serving copy; durability beyond the
//! local disk arrives with the snapshot tier (.features/db.md). What Postgres
//! does hold from day one is the **lease**: a SQLite file is
//! single-writer-single-node (a WAL/shm invariant, not a policy), so opening
//! a database requires holding the (owner, env) lease — trivially satisfied
//! at one instance, and exactly the schema an N>1 deployment needs.
//!
//! Tenant SQL is the product here, so enforcement is structural: the worst a
//! hostile query can do is spoil its own file. The authorizer refuses
//! ATTACH/DETACH and pragmas; a progress handler wired to the invocation
//! deadline stops runaway queries; `max_page_count` makes the size cap a
//! property of the database itself, so overflow fails the tenant's write,
//! never the platform.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgPool;
use uuid::Uuid;

/// v1 size cap for every plan; becomes a per-plan limit once plan versioning
/// carries it (plans are immutable rows, so a new limit means new versions).
pub const DB_MAX_BYTES: i64 = 64 * 1024 * 1024;
const PAGE_SIZE: i64 = 4096;
/// Progress-handler granularity: check the deadline every N VM ops.
const PROGRESS_OPS: i32 = 4_000;
/// Result-set guard: a SELECT can return at most this many rows.
const MAX_ROWS: usize = 10_000;
/// A lease this stale is abandoned and may be taken over.
const LEASE_STALE: &str = "30 seconds";

/// One open database: the connection behind its per-db lock.
type DbHandle = Arc<Mutex<Connection>>;

pub struct DbHost {
    dir: PathBuf,
    /// This process's identity in the lease table.
    instance: String,
    pool: PgPool,
    handles: Mutex<HashMap<(Uuid, String), DbHandle>>,
}

/// The ops the glue sends: `context.db.query/exec/transaction`.
#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
enum DbOp {
    #[serde(rename = "query")]
    Query {
        sql: String,
        #[serde(default)]
        params: Vec<Value>,
    },
    #[serde(rename = "exec")]
    Exec {
        sql: String,
        #[serde(default)]
        params: Vec<Value>,
    },
    #[serde(rename = "transaction")]
    Transaction {
        #[serde(default)]
        statements: Vec<(String, Vec<Value>)>,
    },
}

impl DbHost {
    pub fn new(dir: PathBuf, pool: PgPool) -> DbHost {
        DbHost {
            dir,
            instance: uuid::Uuid::new_v4().to_string(),
            pool,
            handles: Mutex::new(HashMap::new()),
        }
    }

    /// Executes one glue op for (owner, env), bounded by `deadline`.
    pub async fn run(
        &self,
        owner: Uuid,
        env: &str,
        op_json: String,
        deadline: Instant,
    ) -> Result<String, String> {
        let op: DbOp = serde_json::from_str(&op_json).map_err(|e| format!("bad db op: {e}"))?;
        let conn = self.handle(owner, env).await?;
        // rusqlite is synchronous; ops are microseconds in the normal case and
        // deadline-bounded in the hostile one, so run them off the async
        // threads rather than blocking a runtime worker until the deadline.
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.progress_handler(PROGRESS_OPS, Some(move || Instant::now() >= deadline));
            let result = match op {
                DbOp::Query { sql, params } => run_query(&conn, &sql, &params),
                DbOp::Exec { sql, params } => run_exec(&conn, &sql, &params),
                DbOp::Transaction { statements } => run_transaction(&conn, &statements),
            };
            conn.progress_handler(PROGRESS_OPS, None::<fn() -> bool>);
            result
        })
        .await
        .map_err(|e| format!("db task failed: {e}"))?
    }

    /// The open (and lease-checked) connection for (owner, env).
    async fn handle(&self, owner: Uuid, env: &str) -> Result<DbHandle, String> {
        if let Some(hit) = self.handles.lock().unwrap().get(&(owner, env.to_string())) {
            return Ok(hit.clone());
        }
        self.acquire_lease(owner, env).await?;
        let path = self.dir.join(format!("{owner}-{env}.sqlite"));
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create database directory: {e}"))?;
        let conn = open_configured(&path)?;
        let handle = Arc::new(Mutex::new(conn));
        self.handles
            .lock()
            .unwrap()
            .insert((owner, env.to_string()), handle.clone());
        Ok(handle)
    }

    /// Bytes on disk for (owner, env)'s database — 0 when it doesn't exist
    /// yet. Includes the WAL, which is where recent writes live.
    pub fn size_on_disk(&self, owner: Uuid, env: &str) -> u64 {
        let base = self.dir.join(format!("{owner}-{env}.sqlite"));
        let wal = self.dir.join(format!("{owner}-{env}.sqlite-wal"));
        let len = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        len(&base) + len(&wal)
    }

    /// Takes or refreshes the (owner, env) lease; refuses if another live
    /// instance holds it. Trivially ours at N=1 — the check is the schema
    /// the N>1 world needs, kept honest from day one.
    async fn acquire_lease(&self, owner: Uuid, env: &str) -> Result<(), String> {
        // AssertSqlSafe: {LEASE_STALE} interpolates a compile-time constant.
        let taken: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "INSERT INTO db_leases (user_id, env, instance, heartbeat_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (user_id, env) DO UPDATE
                 SET instance = EXCLUDED.instance, heartbeat_at = now()
                 WHERE db_leases.instance = EXCLUDED.instance
                    OR db_leases.heartbeat_at < now() - interval '{LEASE_STALE}'
             RETURNING instance"
        )))
        .bind(owner)
        .bind(env)
        .bind(&self.instance)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("db lease: {e}"))?;
        match taken {
            Some(_) => Ok(()),
            None => Err("this database is currently owned by another instance".to_string()),
        }
    }
}

fn open_configured(path: &std::path::Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("cannot open database: {e}"))?;
    let pragmas = [
        ("journal_mode", "WAL".to_string()),
        ("synchronous", "NORMAL".to_string()),
        ("foreign_keys", "ON".to_string()),
        ("max_page_count", (DB_MAX_BYTES / PAGE_SIZE).to_string()),
    ];
    for (name, value) in pragmas {
        conn.pragma_update(None, name, &value)
            .map_err(|e| format!("db setup ({name}): {e}"))?;
    }
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|e| format!("db setup (busy_timeout): {e}"))?;
    // Installed after our own pragmas: tenant SQL cannot re-attach, load
    // extensions, or flip journal/sync settings out from under the host.
    conn.authorizer(Some(
        |ctx: rusqlite::hooks::AuthContext<'_>| -> rusqlite::hooks::Authorization {
            use rusqlite::hooks::AuthAction;
            match ctx.action {
                AuthAction::Attach { .. } | AuthAction::Detach { .. } => {
                    rusqlite::hooks::Authorization::Deny
                }
                AuthAction::Pragma { .. } => rusqlite::hooks::Authorization::Deny,
                _ => rusqlite::hooks::Authorization::Allow,
            }
        },
    ));
    Ok(conn)
}

/// JSON param → SQLite value. Objects and arrays are refused rather than
/// silently stringified — a caller who wants JSON in a column stringifies it
/// deliberately.
fn bind_params(params: &[Value]) -> Result<Vec<rusqlite::types::Value>, String> {
    use rusqlite::types::Value as Sq;
    params
        .iter()
        .map(|p| match p {
            Value::Null => Ok(Sq::Null),
            Value::Bool(b) => Ok(Sq::Integer(*b as i64)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Sq::Integer(i))
                } else {
                    Ok(Sq::Real(n.as_f64().unwrap_or(0.0)))
                }
            }
            Value::String(s) => Ok(Sq::Text(s.clone())),
            _ => Err("db params must be strings, numbers, booleans, or null".to_string()),
        })
        .collect()
}

fn value_to_json(value: rusqlite::types::ValueRef<'_>) -> Value {
    use base64::Engine as _;
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        // Blobs travel as base64; columns meant for structure should be TEXT.
        ValueRef::Blob(b) => json!(base64::engine::general_purpose::STANDARD.encode(b)),
    }
}

fn run_query(conn: &Connection, sql: &str, params: &[Value]) -> Result<String, String> {
    let bound = bind_params(params)?;
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|n| n.to_string())
        .collect();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(bound))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if out.len() >= MAX_ROWS {
            return Err(format!(
                "query returned more than {MAX_ROWS} rows — add a LIMIT"
            ));
        }
        let mut object = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            object.insert(
                name.clone(),
                value_to_json(row.get_ref(i).map_err(|e| e.to_string())?),
            );
        }
        out.push(Value::Object(object));
    }
    Ok(json!({ "rows": out }).to_string())
}

fn run_exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<String, String> {
    let bound = bind_params(params)?;
    let changes = conn
        .execute(sql, rusqlite::params_from_iter(bound))
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "changes": changes,
        "lastInsertRowid": conn.last_insert_rowid(),
    })
    .to_string())
}

/// An atomic batch: every statement or none. The connection lock is held for
/// the whole (synchronous) batch — there is deliberately no callback form
/// that could hold it across awaits.
fn run_transaction(
    conn: &Connection,
    statements: &[(String, Vec<Value>)],
) -> Result<String, String> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| e.to_string())?;
    let mut changes = 0usize;
    for (sql, params) in statements {
        let bound = match bind_params(params) {
            Ok(bound) => bound,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        };
        match conn.execute(sql, rusqlite::params_from_iter(bound)) {
            Ok(n) => changes += n,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.to_string());
            }
        }
    }
    conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
    Ok(json!({ "changes": changes }).to_string())
}
