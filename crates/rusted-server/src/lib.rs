//! Orchestrator: Postgres-backed store, router, executor pool.

use std::net::SocketAddr;
use std::sync::Arc;

pub mod analytics;
pub mod api;
pub mod auth;
pub mod bundler;
pub mod device;
pub mod local;
pub mod plans;
pub mod state;
pub mod store;
pub mod testsupport;
pub mod tiers;
pub mod web;

use sqlx::postgres::PgPoolOptions;
use state::AppState;
use store::Store;

/// Default for [`ServerConfig::queue_wait_ms`].
pub const DEFAULT_QUEUE_WAIT_MS: u64 = 1500;
/// The compose-provided local database (docker-compose.yml, `make db`).
pub const DEFAULT_DATABASE_URL: &str = "postgres://rusted:rusted@127.0.0.1:5457/rusted";

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Port for the data API serving functions (0 = ephemeral).
    pub data_port: u16,
    /// Port for the admin API + web console (0 = ephemeral).
    pub admin_port: u16,
    /// How long a queued invocation waits for its function's turn before 429.
    pub queue_wait_ms: u64,
    /// Print per-invocation details (outcome, errors, console output, timings)
    /// to the server's stdout.
    pub debug: bool,
    pub database_url: String,
    /// Require `Authorization: Bearer rk_live_…` on the data plane.
    pub require_auth: bool,
}

/// A running server. Both listeners stop when this is dropped.
pub struct ServerHandle {
    pub data_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn db_error(url: &str, e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!(
        "cannot reach postgres at {url}: {e}\nrun `make db` to start the local database"
    ))
}

pub async fn start(config: ServerConfig) -> std::io::Result<ServerHandle> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&config.database_url)
        .await
        .map_err(|e| db_error(&config.database_url, e))?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|e| db_error(&config.database_url, e))?;
    let store = Store::new(pool.clone());
    let (recorder, analytics_writer) = analytics::start(pool.clone());
    let state = Arc::new(AppState::new(
        store,
        pool.clone(),
        recorder,
        config.queue_wait_ms,
        config.debug,
        config.require_auth,
    ));

    let data_listener = tokio::net::TcpListener::bind(("127.0.0.1", config.data_port)).await?;
    let admin_listener = tokio::net::TcpListener::bind(("127.0.0.1", config.admin_port)).await?;
    let data_addr = data_listener.local_addr()?;
    let admin_addr = admin_listener.local_addr()?;
    state
        .data_addr
        .set(data_addr)
        .expect("data_addr set exactly once");
    state
        .admin_addr
        .set(admin_addr)
        .expect("admin_addr set exactly once");

    let data_app = api::data_router(state.clone());
    let admin_app =
        api::admin_router(state.clone()).merge(web::router(web::WebState::new(state.clone())));
    let tasks = vec![
        tokio::spawn(async move {
            axum::serve(data_listener, data_app)
                .await
                .expect("data listener serves until aborted");
        }),
        tokio::spawn(async move {
            axum::serve(admin_listener, admin_app)
                .await
                .expect("admin listener serves until aborted");
        }),
        tokio::spawn(api::sweep_temp_runs(state.clone())),
        tokio::spawn(auth::invalidation_listener(
            config.database_url.clone(),
            state.clone(),
        )),
        analytics_writer,
        tokio::spawn(analytics::retention_sweeper(pool)),
    ];

    Ok(ServerHandle {
        data_addr,
        admin_addr,
        tasks,
    })
}
