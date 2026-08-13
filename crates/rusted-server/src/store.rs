//! Postgres-backed function store: content-addressed `artifacts` plus
//! `functions`/`revisions`, with an in-memory read cache invalidated over
//! LISTEN/NOTIFY (`function:<name>` on channel `rusted_invalidations`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub const INVALIDATION_CHANNEL: &str = "rusted_invalidations";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub rev: u64,
    pub hash: String,
    pub created_at: u64,
}

/// How a function is exposed over HTTP: which methods it accepts and an
/// optional route pattern nested under `/f/<name>` (e.g. `/users/{id}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpTrigger {
    pub methods: Vec<String>,
    #[serde(default)]
    pub path: Option<String>,
}

impl Default for HttpTrigger {
    fn default() -> Self {
        Self {
            methods: vec!["POST".to_string()],
            path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionRecord {
    pub revisions: Vec<Revision>,
    pub current_rev: u64,
    #[serde(default)]
    pub trigger: HttpTrigger,
    /// Carried on the record so the cached read serves plan lookups too —
    /// the invocation path must not query Postgres.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Data-plane protocol: `"http"` or `"mcp"`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Deploy-time MCP tool metadata (handlers stripped); NULL for http.
    #[serde(default)]
    pub mcp: Option<serde_json::Value>,
    /// Secret names the module requested via `export const config`, captured
    /// at deploy time so invocation needs no re-inspection.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Callable without an API key even under --require-auth. Captured at
    /// deploy time from the module's own declaration, for either kind.
    #[serde(default)]
    pub public: bool,
    /// Whether the module declared `config.state = true`.
    #[serde(default)]
    pub state: bool,
    /// Declared object bindings (`config.objects`), as stored JSONB — secret
    /// NAMES only, never credentials.
    #[serde(default)]
    pub objects: Option<serde_json::Value>,
    /// Operational serving toggle — console-controlled, untouched by pushes.
    #[serde(default = "default_published")]
    pub published: bool,
}

fn default_published() -> bool {
    true
}

/// Deploy-time facts captured from the module's own declarations — everything
/// a push writes besides source, trigger, and kind. Always taken from the
/// freshly inspected source, never preserved from an earlier revision.
#[derive(Debug, Default, Clone)]
pub struct Declared {
    pub secrets: Vec<String>,
    pub public: bool,
    pub state: bool,
    pub objects: std::collections::BTreeMap<String, rusted_engine::ObjectBinding>,
}

impl Declared {
    pub fn from_config(config: &rusted_engine::RuntimeConfig, public: bool) -> Declared {
        Declared {
            secrets: config.secrets.clone(),
            public,
            state: config.wants_state(),
            objects: config.objects.clone(),
        }
    }
}

fn default_kind() -> String {
    "http".to_string()
}

impl FunctionRecord {
    pub fn current(&self) -> &Revision {
        self.revisions
            .iter()
            .find(|r| r.rev == self.current_rev)
            .expect("current_rev always references an existing revision")
    }
}

/// Snapshot of a function's current revision, served through the read cache —
/// everything the invocation path needs without touching Postgres.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub source: String,
    pub trigger: HttpTrigger,
    pub owner: Option<Uuid>,
    pub kind: String,
    pub mcp: Option<serde_json::Value>,
    /// Secret names to decrypt into `context.env` for every invocation.
    pub secrets: Vec<String>,
    /// Whether the auth gate lets keyless callers through to this function.
    pub public: bool,
    /// Whether `context.state` was declared.
    pub state: bool,
    /// Whether the owner has this function on the air.
    pub published: bool,
    /// Declared object bindings, parsed once here so the invocation path
    /// never re-reads JSON.
    pub objects: std::collections::BTreeMap<String, rusted_engine::ObjectBinding>,
    pub rev: u64,
}

pub struct Store {
    pool: PgPool,
    /// name → current-revision snapshot; the hot path for serving functions.
    cache: Mutex<HashMap<String, Arc<Fetched>>>,
}

impl Store {
    pub fn new(pool: PgPool) -> Store {
        Store {
            pool,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn invalidate(&self, name: &str) {
        self.cache.lock().unwrap().remove(name);
    }

    pub fn invalidate_all(&self) {
        self.cache.lock().unwrap().clear();
    }

    /// Number of functions a user owns — the plan's function-count limit.
    pub async fn count_for_user(&self, user_id: Uuid) -> sqlx::Result<i64> {
        Ok(
            sqlx::query("SELECT count(*) AS n FROM functions WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?
                .get("n"),
        )
    }

    /// The function's owner, if it has one.
    pub async fn owner(&self, name: &str) -> sqlx::Result<Option<Uuid>> {
        Ok(sqlx::query("SELECT user_id FROM functions WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .and_then(|row| row.get::<Option<Uuid>, _>("user_id")))
    }

    pub async fn names_for_user(&self, user_id: Uuid) -> sqlx::Result<Vec<String>> {
        let rows = sqlx::query("SELECT name FROM functions WHERE user_id = $1 ORDER BY name")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get("name")).collect())
    }

    /// Push keeping the function's existing trigger (default for new functions).
    /// Note the asymmetry: the trigger is preserved, but kind and mcp metadata
    /// are always reset to http/NULL — kind derives from the deployed source,
    /// so callers deploying an mcp module must use `push_full`. The declared
    /// facts (secrets, publicness, capabilities) always come from the deployed
    /// source too, so they are never preserved.
    pub async fn push(
        &self,
        name: &str,
        source: &str,
        user_id: Option<Uuid>,
        declared: &Declared,
    ) -> sqlx::Result<Revision> {
        let existing: Option<HttpTrigger> =
            sqlx::query("SELECT methods, path FROM functions WHERE name = $1")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
                .map(|row| HttpTrigger {
                    methods: row.get("methods"),
                    path: row.get("path"),
                });
        self.push_with_trigger(
            name,
            source,
            existing.unwrap_or_default(),
            user_id,
            declared,
        )
        .await
    }

    /// Push an http function with an explicit trigger.
    pub async fn push_with_trigger(
        &self,
        name: &str,
        source: &str,
        trigger: HttpTrigger,
        user_id: Option<Uuid>,
        declared: &Declared,
    ) -> sqlx::Result<Revision> {
        self.push_full(
            name,
            source,
            Some(&trigger),
            "http",
            None,
            user_id,
            declared,
        )
        .await
    }

    /// The full push: kind selects the data-plane protocol (`"http"` or
    /// `"mcp"`), mcp carries the tool metadata for mcp functions. A missing
    /// trigger stores the default (mcp functions ignore it).
    #[allow(clippy::too_many_arguments)]
    pub async fn push_full(
        &self,
        name: &str,
        source: &str,
        trigger: Option<&HttpTrigger>,
        kind: &str,
        mcp: Option<&serde_json::Value>,
        user_id: Option<Uuid>,
        declared: &Declared,
    ) -> sqlx::Result<Revision> {
        let default_trigger = HttpTrigger::default();
        let trigger = trigger.unwrap_or(&default_trigger);
        let hash = hex::encode(Sha256::digest(source.as_bytes()));
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO artifacts (hash, source, size_bytes) VALUES ($1, $2, $3)
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&hash)
        .bind(source)
        .bind(source.len() as i64)
        .execute(&mut *tx)
        .await?;
        let rev: i64 = sqlx::query(
            "SELECT coalesce(max(rev), 0) + 1 AS next FROM revisions WHERE function_name = $1",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await?
        .get("next");
        let objects = if declared.objects.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&declared.objects).expect("bindings serialize"))
        };
        sqlx::query(
            "INSERT INTO functions
                 (name, current_rev, methods, path, user_id, kind, mcp, secrets, public,
                  state, objects)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (name) DO UPDATE
                 SET current_rev = $2, methods = $3, path = $4, updated_at = now(),
                     user_id = coalesce(functions.user_id, $5), kind = $6, mcp = $7,
                     secrets = $8, public = $9, state = $10, objects = $11",
        )
        .bind(name)
        .bind(rev)
        .bind(&trigger.methods)
        .bind(&trigger.path)
        .bind(user_id)
        .bind(kind)
        .bind(mcp)
        .bind(declared.secrets.to_vec())
        .bind(declared.public)
        .bind(declared.state)
        .bind(objects)
        .execute(&mut *tx)
        .await?;
        let created_at: i64 = sqlx::query(
            "INSERT INTO revisions (function_name, rev, artifact_hash) VALUES ($1, $2, $3)
             RETURNING extract(epoch FROM created_at)::bigint AS at",
        )
        .bind(name)
        .bind(rev)
        .bind(&hash)
        .fetch_one(&mut *tx)
        .await?
        .get("at");
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(INVALIDATION_CHANNEL)
            .bind(format!("function:{name}"))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.invalidate(name);
        Ok(Revision {
            rev: rev as u64,
            hash,
            created_at: created_at.max(0) as u64,
        })
    }

    pub async fn names(&self) -> sqlx::Result<Vec<String>> {
        let rows = sqlx::query("SELECT name FROM functions ORDER BY name")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get("name")).collect())
    }

    pub async fn get(&self, name: &str) -> sqlx::Result<Option<FunctionRecord>> {
        let Some(function) = sqlx::query(
            "SELECT current_rev, methods, path, user_id, kind, mcp, secrets, public,
                    state, objects, published
             FROM functions WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let revisions = sqlx::query(
            "SELECT rev, artifact_hash, extract(epoch FROM created_at)::bigint AS at
             FROM revisions WHERE function_name = $1 ORDER BY rev",
        )
        .bind(name)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| Revision {
            rev: row.get::<i64, _>("rev") as u64,
            hash: row.get("artifact_hash"),
            created_at: row.get::<i64, _>("at").max(0) as u64,
        })
        .collect();
        Ok(Some(FunctionRecord {
            revisions,
            current_rev: function.get::<i64, _>("current_rev") as u64,
            trigger: HttpTrigger {
                methods: function.get("methods"),
                path: function.get("path"),
            },
            user_id: function.get("user_id"),
            kind: function.get("kind"),
            mcp: function.get("mcp"),
            secrets: function.get("secrets"),
            public: function.get("public"),
            state: function.get("state"),
            objects: function.get("objects"),
            published: function.get("published"),
        }))
    }

    /// Source of the function's current revision.
    pub async fn source(&self, name: &str) -> sqlx::Result<Option<String>> {
        Ok(self.fetch(name).await?.map(|hit| hit.source.clone()))
    }

    /// Current-revision snapshot through the read cache — the per-request hot
    /// path.
    pub async fn fetch(&self, name: &str) -> sqlx::Result<Option<Arc<Fetched>>> {
        if let Some(hit) = self.cache.lock().unwrap().get(name) {
            return Ok(Some(hit.clone()));
        }
        let Some(record) = self.get(name).await? else {
            return Ok(None);
        };
        let source: String = sqlx::query(
            "SELECT a.source FROM artifacts a
             JOIN revisions r ON r.artifact_hash = a.hash
             WHERE r.function_name = $1 AND r.rev = $2",
        )
        .bind(name)
        .bind(record.current_rev as i64)
        .fetch_one(&self.pool)
        .await?
        .get("source");
        let hit = Arc::new(Fetched {
            source,
            trigger: record.trigger,
            owner: record.user_id,
            kind: record.kind,
            mcp: record.mcp,
            secrets: record.secrets,
            public: record.public,
            state: record.state,
            published: record.published,
            objects: record
                .objects
                .as_ref()
                .and_then(|raw| serde_json::from_value(raw.clone()).ok())
                .unwrap_or_default(),
            rev: record.current_rev,
        });
        self.cache
            .lock()
            .unwrap()
            .insert(name.to_string(), hit.clone());
        Ok(Some(hit))
    }

    /// Flips the serving toggle. Scoped to the owner in SQL so a console bug
    /// can never unpublish somebody else's function.
    pub async fn set_published(
        &self,
        name: &str,
        owner: Uuid,
        published: bool,
    ) -> sqlx::Result<bool> {
        let result = sqlx::query(
            "UPDATE functions SET published = $3, updated_at = now()
             WHERE name = $1 AND user_id = $2",
        )
        .bind(name)
        .bind(owner)
        .bind(published)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(INVALIDATION_CHANNEL)
            .bind(format!("function:{name}"))
            .execute(&self.pool)
            .await?;
        self.invalidate(name);
        Ok(true)
    }

    /// Removes the function (revisions cascade; artifacts stay, content-addressed).
    pub async fn delete(&self, name: &str) -> sqlx::Result<bool> {
        let result = sqlx::query("DELETE FROM functions WHERE name = $1")
            .bind(name)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(INVALIDATION_CHANNEL)
            .bind(format!("function:{name}"))
            .execute(&self.pool)
            .await?;
        self.invalidate(name);
        Ok(true)
    }
}
