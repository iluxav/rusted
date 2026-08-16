use rusted_server::store::{Declared, HttpTrigger, Store};
use rusted_server::testsupport;
use sqlx::Row;

const SRC_A: &str = "export default async function handler() { return \"a\"; }";
const SRC_B: &str = "export default async function handler() { return \"b\"; }";

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

async fn store() -> (Store, sqlx::PgPool, uuid::Uuid) {
    let url = testsupport::create_test_database().await;
    let pool = testsupport::migrate(&url).await;
    let owner = testsupport::seed_user(&pool).await;
    (Store::new(pool.clone()), pool, owner)
}

#[tokio::test]
async fn push_creates_revision_and_artifact() {
    let (store, pool, owner) = store().await;
    let rev = store
        .push("greet", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    assert_eq!(rev.rev, 1);
    assert_eq!(rev.hash, sha256_hex(SRC_A));
    let source: String = sqlx::query("SELECT source FROM artifacts WHERE hash = $1")
        .bind(&rev.hash)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("source");
    assert_eq!(source, SRC_A);
}

#[tokio::test]
async fn identical_source_dedupes_artifact_but_adds_revision() {
    let (store, pool, owner) = store().await;
    let r1 = store
        .push("greet", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    let r2 = store
        .push("greet", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    assert_eq!((r1.rev, r2.rev), (1, 2));
    assert_eq!(r1.hash, r2.hash);
    let artifacts: i64 = sqlx::query("SELECT count(*) AS n FROM artifacts")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(artifacts, 1);
}

#[tokio::test]
async fn new_source_moves_current_revision() {
    let (store, _pool, owner) = store().await;
    store
        .push("greet", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    store
        .push("greet", SRC_B, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    let record = store.get("greet").await.unwrap().unwrap();
    assert_eq!(record.current_rev, 2);
    assert_eq!(record.revisions.len(), 2);
    assert_eq!(store.source("greet").await.unwrap().unwrap(), SRC_B);
}

#[tokio::test]
async fn functions_visible_from_a_second_store_instance() {
    let (store, pool, owner) = store().await;
    store
        .push("greet", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    let other = Store::new(pool);
    assert_eq!(other.names().await.unwrap(), vec!["greet".to_string()]);
    assert_eq!(other.source("greet").await.unwrap().unwrap(), SRC_A);
}

#[tokio::test]
async fn delete_removes_function_but_keeps_artifacts() {
    let (store, pool, owner) = store().await;
    let rev = store
        .push("greet", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    assert!(store.delete("greet").await.unwrap());
    assert!(store.get("greet").await.unwrap().is_none());
    assert!(store.source("greet").await.unwrap().is_none());
    let artifacts: i64 = sqlx::query("SELECT count(*) AS n FROM artifacts WHERE hash = $1")
        .bind(&rev.hash)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(artifacts, 1);
}

#[tokio::test]
async fn delete_missing_function_returns_false() {
    let (store, _pool, _owner) = store().await;
    assert!(!store.delete("ghost").await.unwrap());
}

#[tokio::test]
async fn trigger_config_persists() {
    let (store, pool, owner) = store().await;
    store
        .push_with_trigger(
            "api",
            SRC_A,
            HttpTrigger {
                methods: vec!["GET".into(), "POST".into()],
                path: Some("/users/{id}".into()),
            },
            Some(owner),
            &Declared::default(),
            "cli",
        )
        .await
        .unwrap();
    let other = Store::new(pool);
    let trigger = other.get("api").await.unwrap().unwrap().trigger;
    assert_eq!(trigger.methods, vec!["GET", "POST"]);
    assert_eq!(trigger.path.as_deref(), Some("/users/{id}"));
}

#[tokio::test]
async fn plain_push_preserves_existing_trigger() {
    let (store, _pool, owner) = store().await;
    store
        .push_with_trigger(
            "api",
            SRC_A,
            HttpTrigger {
                methods: vec!["GET".into()],
                path: Some("/ping".into()),
            },
            Some(owner),
            &Declared::default(),
            "cli",
        )
        .await
        .unwrap();
    store
        .push("api", SRC_B, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    let trigger = store.get("api").await.unwrap().unwrap().trigger;
    assert_eq!(trigger.methods, vec!["GET"]);
    assert_eq!(trigger.path.as_deref(), Some("/ping"));
}

#[tokio::test]
async fn mcp_metadata_round_trips() {
    let (store, _pool, owner) = store().await;
    let meta = serde_json::json!({"public": false, "tools": {"slugify": {"description": "d", "inputSchema": {"type": "object"}}}});
    store
        .push_full(
            "m",
            "export const mcp = {}",
            None,
            "mcp",
            Some(&meta),
            None,
            Some(owner),
            &Declared::default(),
            "cli",
        )
        .await
        .unwrap();
    let f = store.fetch("m").await.unwrap().unwrap();
    assert_eq!(f.kind, "mcp");
    assert_eq!(
        f.mcp.as_ref().unwrap()["tools"]["slugify"]["description"],
        "d"
    );
    assert_eq!(f.rev, 1);
}

#[tokio::test]
async fn redeploying_as_http_clears_mcp_metadata() {
    let (store, _pool, owner) = store().await;
    let meta = serde_json::json!({"public": true, "tools": {}});
    store
        .push_full(
            "flip",
            "export const mcp = {}",
            None,
            "mcp",
            Some(&meta),
            None,
            Some(owner),
            &Declared::default(),
            "cli",
        )
        .await
        .unwrap();
    // A plain push always derives kind from the source being deployed, so the
    // mcp metadata must not survive the function's return to http.
    store
        .push("flip", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    let f = store.fetch("flip").await.unwrap().unwrap();
    assert_eq!(f.kind, "http");
    assert!(f.mcp.is_none());
    assert_eq!(f.rev, 2);
}

#[tokio::test]
async fn fetch_caches_and_own_pushes_invalidate() {
    let (store, _pool, owner) = store().await;
    store
        .push("greet", SRC_A, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    let first = store.fetch("greet").await.unwrap().unwrap();
    assert_eq!(first.source, SRC_A);
    assert_eq!(first.kind, "http");
    assert!(first.mcp.is_none());
    store
        .push("greet", SRC_B, Some(owner), &Declared::default(), "cli")
        .await
        .unwrap();
    let second = store.fetch("greet").await.unwrap().unwrap();
    assert_eq!(second.source, SRC_B);
}
