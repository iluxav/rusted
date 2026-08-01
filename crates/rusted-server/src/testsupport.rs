//! Helpers for tests: disposable per-test databases on the compose Postgres.
//! Not used by the production binary.

use sqlx::postgres::PgPoolOptions;

/// Server base (no database name) for the compose instance; override with
/// `RUSTED_TEST_DB_BASE` if your Postgres lives elsewhere.
fn base_url() -> String {
    std::env::var("RUSTED_TEST_DB_BASE")
        .unwrap_or_else(|_| "postgres://rusted:rusted@127.0.0.1:5457".to_string())
}

/// Creates a fresh `rusted_test_<n>` database and returns its URL. Migrations
/// are NOT run — the server runs them at boot; store-level tests call
/// [`migrate`].
///
/// Test databases are dropped by `make test` (before and after the run) rather
/// than at the end of each test: pools outlive the test body, so an in-test
/// DROP would race live connections. Left unchecked they fill the Postgres
/// volume — `make db-clean` is the manual escape hatch.
pub async fn create_test_database() -> String {
    let base = base_url();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{base}/postgres"))
        .await
        .expect("cannot reach postgres — run `make db` first");
    // pid + counter + clock: the clock alone is NOT unique (macOS SystemTime
    // has microsecond resolution and parallel tests collide within it).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "rusted_test_{}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros()
    );
    // Concurrent CREATE DATABASE calls can collide on the template lock;
    // retry briefly instead of failing the test.
    let mut attempts = 0;
    loop {
        match sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
            .execute(&admin)
            .await
        {
            Ok(_) => break,
            Err(e) if attempts < 20 => {
                attempts += 1;
                let _ = e;
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            Err(e) => panic!("create test database: {e}"),
        }
    }
    format!("{base}/{name}")
}

/// Connects to `url` and applies the server's migrations.
pub async fn migrate(url: &str) -> sqlx::PgPool {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .expect("connect to test database");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations apply");
    pool
}

/// Inserts a user and returns its id — for tests that need key/session rows.
pub async fn seed_user(pool: &sqlx::PgPool) -> uuid::Uuid {
    use sqlx::Row;
    // Unique per call: a database may host several test users.
    static SEQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(4242);
    let github_id = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    sqlx::query("INSERT INTO users (github_id, login) VALUES ($1, $2) RETURNING id")
        .bind(github_id)
        .bind(format!("test-user-{github_id}"))
        .fetch_one(pool)
        .await
        .expect("seed user")
        .get("id")
}
