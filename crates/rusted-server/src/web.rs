//! The rusted console: htmx + tailwind pages served from the admin listener.
//! Backed by Postgres: real GitHub OAuth sessions, real API keys, live lambda
//! data. The dashboard keeps labeled sample data until analytics land.

use std::sync::Arc;
use std::time::Instant;

use askama::Template;
use axum::extract::{Form, Path, Query, RawQuery, State};
use axum::http::header::{self, HeaderMap, SET_COOKIE};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::auth::{self, User};
use crate::state::{now_epoch, AppState};

// ------------------------------------------------------------------- state

pub struct WebInner {
    app: Arc<AppState>,
    http: reqwest::Client,
    oauth: Option<OauthConfig>,
    google: Option<GoogleConfig>,
}

#[derive(Clone)]
pub struct WebState(Arc<WebInner>);

pub struct OauthConfig {
    client_id: String,
    client_secret: String,
    callback_url: String,
}

impl OauthConfig {
    /// Platform sign-in credentials come from the RUSTED_CONSOLE_* variables
    /// — operator configuration, deliberately namespaced so it can never be
    /// confused with the identically-spelled tenant secrets that live in the
    /// vault (a Renote-style function's GITHUB_CLIENT_ID is a different app
    /// in a different trust domain). The bare names remain as a fallback so
    /// existing deployments keep signing in.
    fn from_env() -> Option<OauthConfig> {
        let var = |name: &str| {
            std::env::var(format!("RUSTED_CONSOLE_{name}"))
                .or_else(|_| std::env::var(name))
                .ok()
        };
        Some(OauthConfig {
            client_id: var("GITHUB_CLIENT_ID")?,
            client_secret: var("GITHUB_CLIENT_SECRET")?,
            callback_url: var("GITHUB_CALLBACK_URL")
                .unwrap_or_else(|| "http://127.0.0.1:7412/auth/github/callback".to_string()),
        })
    }
}

pub struct GoogleConfig {
    client_id: String,
    client_secret: String,
    callback_url: String,
}

impl GoogleConfig {
    /// Operator configuration, same namespacing rule as the GitHub pair —
    /// platform sign-in is never a tenant concern. No bare-name fallback:
    /// this feature is younger than the naming convention.
    fn from_env() -> Option<GoogleConfig> {
        Some(GoogleConfig {
            client_id: std::env::var("RUSTED_CONSOLE_GOOGLE_CLIENT_ID").ok()?,
            client_secret: std::env::var("RUSTED_CONSOLE_GOOGLE_CLIENT_SECRET").ok()?,
            callback_url: std::env::var("RUSTED_CONSOLE_GOOGLE_CALLBACK_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:7412/auth/google/callback".to_string()),
        })
    }
}

impl WebState {
    pub fn new(app: Arc<AppState>) -> Self {
        WebState(Arc::new(WebInner {
            app,
            http: reqwest::Client::new(),
            oauth: OauthConfig::from_env(),
            google: GoogleConfig::from_env(),
        }))
    }
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .merge(crate::assets::router())
        .route("/", get(landing))
        .route("/docs", get(docs_index))
        .route("/docs/{page}", get(docs_page))
        .route("/robots.txt", get(robots_txt))
        .route("/sitemap.xml", get(sitemap_xml))
        .route("/llms.txt", get(llms_txt))
        .route("/llms-full.txt", get(llms_full_txt))
        .route("/login", get(login))
        .route(
            "/oauth/authorize",
            get(oauth_authorize).post(oauth_authorize_decide),
        )
        .route("/auth/github", get(auth_github))
        .route("/auth/github/callback", get(auth_github_callback))
        .route("/auth/google", get(auth_google))
        .route("/auth/google/callback", get(auth_google_callback))
        .route("/logout", get(logout))
        .route("/device", get(device_page).post(device_decide))
        .route("/console", get(console_home))
        .route("/console/dashboard", get(page_dashboard))
        .route("/console/invocations", get(page_invocations))
        .route("/console/keys", get(page_keys).post(key_create))
        .route("/console/secrets", get(page_secrets).post(secret_set))
        .route("/console/secrets/{env}/{name}", delete(secret_delete))
        .route("/console/environments", post(environment_create))
        .route("/console/environments/{name}", delete(environment_delete))
        .route("/console/billing", get(page_billing))
        .route(
            "/console/checkout/{code}",
            get(page_checkout).post(confirm_checkout),
        )
        .route("/console/keys/{id}", delete(key_revoke))
        .route(
            "/console/function/{name}",
            get(page_function).delete(function_delete),
        )
        .route("/console/function/{name}/published", post(function_publish))
        .route("/console/database", get(page_database))
        .route("/console/database/sql", post(database_sql))
        .route("/console/editor", get(page_editor))
        .route("/console/nav/functions", get(nav_functions))
        .route("/console/editor/run", post(editor_run))
        .route("/console/editor/verify", post(editor_verify))
        .route("/console/editor/push", post(editor_push))
        .route("/console/admin", get(page_admin))
        .route("/console/admin/users", get(page_admin_users))
        .route("/console/admin/users/{id}/admin", post(admin_toggle_admin))
        .route("/console/admin/functions", get(page_admin_functions))
        // The short spelling lands on the console section.
        .route("/admin", get(|| async { Redirect::to("/console/admin") }))
        // The old spelling redirects — bookmarks and muscle memory keep
        // working, and htmx requests land on the canonical page.
        .route("/console/lambda/{name}", get(legacy_lambda_redirect))
        .route("/console/test", post(run_test))
        .with_state(state)
}

// ------------------------------------------------------------------- oauth

fn cookie_value<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

async fn current_user(state: &WebState, headers: &HeaderMap) -> Option<User> {
    let token = cookie_value(headers, auth::SESSION_COOKIE)?;
    auth::resolve_session(&state.0.app, token).await
}

async fn auth_github(State(state): State<WebState>) -> Response {
    let Some(oauth) = &state.0.oauth else {
        return Redirect::to("/login").into_response();
    };
    let csrf = auth::random_token(16);
    let authorize = format!(
        "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=read:user%20user:email&state={csrf}",
        oauth.client_id, oauth.callback_url,
    );
    (
        [(
            SET_COOKIE,
            format!("rusted_oauth_state={csrf}; Path=/; HttpOnly; Max-Age=600; SameSite=Lax"),
        )],
        Redirect::to(&authorize),
    )
        .into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct GithubToken {
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct GithubUser {
    id: i64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    /// Only set when the profile email is public; the real source is
    /// /user/emails, which the user:email scope unlocks.
    email: Option<String>,
}

async fn auth_google(State(state): State<WebState>) -> Response {
    let Some(google) = &state.0.google else {
        return Redirect::to("/login").into_response();
    };
    let csrf = auth::random_token(16);
    let authorize = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope=openid%20email%20profile&state={csrf}",
        google.client_id, google.callback_url,
    );
    (
        [(
            SET_COOKIE,
            format!("rusted_oauth_state={csrf}; Path=/; HttpOnly; Max-Age=600; SameSite=Lax"),
        )],
        Redirect::to(&authorize),
    )
        .into_response()
}

#[derive(Deserialize)]
struct GoogleToken {
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct GoogleClaims {
    iss: String,
    aud: String,
    sub: String,
    exp: u64,
    email: Option<String>,
    #[serde(default)]
    email_verified: bool,
    name: Option<String>,
    picture: Option<String>,
}

/// Reads the claims out of an id_token that came to us directly from
/// Google's token endpoint over TLS, authenticated by our client secret.
/// In that flow the channel vouches for the token (OIDC core §3.1.3.7
/// explicitly permits this), so no JWKS fetch — but issuer, audience, and
/// expiry are still checked: a token for someone else's client id must not
/// sign anyone in here.
fn parse_google_claims(id_token: &str, client_id: &str, now: u64) -> Option<GoogleClaims> {
    use base64::Engine;
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: GoogleClaims = serde_json::from_slice(&bytes).ok()?;
    let issuer_ok =
        claims.iss == "https://accounts.google.com" || claims.iss == "accounts.google.com";
    (issuer_ok && claims.aud == client_id && claims.exp > now).then_some(claims)
}

async fn auth_google_callback(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(google) = &state.0.google else {
        return Redirect::to("/login").into_response();
    };
    if cookie_value(&headers, "rusted_oauth_state") != Some(query.state.as_str()) {
        return login_error("sign-in state mismatch — try again");
    }
    let token: GoogleToken = match state
        .0
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", query.code.as_str()),
            ("client_id", google.client_id.as_str()),
            ("client_secret", google.client_secret.as_str()),
            ("redirect_uri", google.callback_url.as_str()),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(response) => match response.json().await {
            Ok(token) => token,
            Err(e) => return login_error(&format!("google token response unreadable: {e}")),
        },
        Err(e) => return login_error(&format!("google token exchange failed: {e}")),
    };
    let Some(id_token) = token.id_token else {
        return login_error("google rejected the sign-in code — try again");
    };
    let Some(claims) = parse_google_claims(&id_token, &google.client_id, now_epoch()) else {
        return login_error("google returned a token this server does not accept");
    };
    // Unverified addresses neither store nor link — see OauthProfile::email.
    let email = claims.email.as_deref().filter(|_| claims.email_verified);
    let login = email
        .and_then(|e| e.split('@').next())
        .or(claims.name.as_deref())
        .unwrap_or("google-user");
    let pool = &state.0.app.pool;
    let user_id = match auth::resolve_oauth_user(
        pool,
        auth::OauthProfile {
            provider: "google",
            subject: claims.sub.clone(),
            login,
            name: claims.name.as_deref(),
            avatar_url: claims.picture.as_deref(),
            email,
        },
    )
    .await
    {
        Ok(id) => id,
        Err(e) => return login_error(&format!("saving your account failed: {e}")),
    };
    let session = match auth::create_session(pool, user_id).await {
        Ok(token) => token,
        Err(e) => return login_error(&format!("creating your session failed: {e}")),
    };
    let next = cookie_value(&headers, "rusted_after_login")
        .filter(|n| n.starts_with('/') && !n.starts_with("//") && *n != "/login")
        .unwrap_or("/console")
        .to_string();
    login_success_response(&session, &next)
}

#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

/// The address GitHub considers the account's own: primary and verified
/// first, any verified one second, nothing otherwise — an unverified string
/// is a claim, not an address.
fn pick_github_email(mut emails: Vec<GithubEmail>) -> Option<String> {
    emails.retain(|e| e.verified);
    emails
        .iter()
        .position(|e| e.primary)
        .map(|i| emails.swap_remove(i).email)
        .or_else(|| emails.into_iter().next().map(|e| e.email))
}

async fn auth_github_callback(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(oauth) = &state.0.oauth else {
        return Redirect::to("/login").into_response();
    };
    if cookie_value(&headers, "rusted_oauth_state") != Some(query.state.as_str()) {
        return login_error("sign-in state mismatch — try again");
    }
    let token: GithubToken = match state
        .0
        .http
        .post("https://github.com/login/oauth/access_token")
        .header("accept", "application/json")
        .form(&[
            ("client_id", oauth.client_id.as_str()),
            ("client_secret", oauth.client_secret.as_str()),
            ("code", query.code.as_str()),
        ])
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(response) => match response.json().await {
            Ok(token) => token,
            Err(e) => return login_error(&format!("github token response unreadable: {e}")),
        },
        Err(e) => return login_error(&format!("github token exchange failed: {e}")),
    };
    let Some(access_token) = token.access_token else {
        return login_error("github rejected the sign-in code — try again");
    };
    let gh_user: GithubUser = match state
        .0
        .http
        .get("https://api.github.com/user")
        .bearer_auth(&access_token)
        .header("user-agent", "rusted-console")
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(response) => match response.json().await {
            Ok(user) => user,
            Err(e) => return login_error(&format!("github profile unreadable: {e}")),
        },
        Err(e) => return login_error(&format!("github profile fetch failed: {e}")),
    };
    // The profile's public email is often absent; /user/emails has the real
    // list. Best-effort — a login must not fail because this endpoint did.
    let email = match gh_user.email.clone() {
        Some(public) => Some(public),
        None => {
            let response = state
                .0
                .http
                .get("https://api.github.com/user/emails")
                .bearer_auth(&access_token)
                .header("user-agent", "rusted-console")
                .send()
                .await
                .and_then(|r| r.error_for_status());
            match response {
                Ok(r) => r
                    .json::<Vec<GithubEmail>>()
                    .await
                    .ok()
                    .and_then(pick_github_email),
                Err(_) => None,
            }
        }
    };
    let pool = &state.0.app.pool;
    let user_id = match auth::resolve_oauth_user(
        pool,
        auth::OauthProfile {
            provider: "github",
            subject: gh_user.id.to_string(),
            login: &gh_user.login,
            name: gh_user.name.as_deref(),
            avatar_url: gh_user.avatar_url.as_deref(),
            email: email.as_deref(),
        },
    )
    .await
    {
        Ok(id) => id,
        Err(e) => return login_error(&format!("saving your account failed: {e}")),
    };
    let session = match auth::create_session(pool, user_id).await {
        Ok(token) => token,
        Err(e) => return login_error(&format!("creating your session failed: {e}")),
    };
    let next = cookie_value(&headers, "rusted_after_login")
        .filter(|n| n.starts_with('/') && !n.starts_with("//") && *n != "/login")
        .unwrap_or("/console")
        .to_string();
    login_success_response(&session, &next)
}

/// The response that finishes a sign-in: the session cookie, the
/// after-login cleanup, and the redirect home.
///
/// `AppendHeaders`, not a plain header array, and that is the whole bug this
/// helper exists to pin: axum's array form *inserts* headers, so a second
/// Set-Cookie silently replaced the session cookie — every login succeeded
/// server-side and the browser never received it.
fn login_success_response(session: &str, next: &str) -> Response {
    use axum::response::AppendHeaders;
    (
        AppendHeaders([
            (
                SET_COOKIE,
                format!(
                    "{}={session}; Path=/; HttpOnly; Max-Age=2592000; SameSite=Lax; Secure",
                    auth::SESSION_COOKIE
                ),
            ),
            (
                SET_COOKIE,
                "rusted_after_login=; Path=/; Max-Age=0".to_string(),
            ),
        ]),
        Redirect::to(next),
    )
        .into_response()
}

fn login_error(message: &str) -> Response {
    Html(format!(
        r#"<!doctype html><meta http-equiv="refresh" content="4;url=/login">
           <body style="background:#23100A;color:#F4E3D0;font-family:monospace;display:grid;place-items:center;height:100vh">
           <p>{message}<br><br>returning to sign-in…</p></body>"#
    ))
    .into_response()
}

async fn logout(State(state): State<WebState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, auth::SESSION_COOKIE) {
        auth::destroy_session(&state.0.app.pool, &state.0.app.auth, token).await;
    }
    (
        [(
            SET_COOKIE,
            format!("{}=; Path=/; Max-Age=0", auth::SESSION_COOKIE),
        )],
        Redirect::to("/"),
    )
        .into_response()
}

// ------------------------------------------------------------ device sign-in

#[derive(Deserialize)]
struct DeviceQuery {
    #[serde(default)]
    code: String,
}

/// Signing in first means the approval screen always knows who is approving.
fn device_login_redirect(code: &str) -> Response {
    Redirect::to(&format!("/login?next=/device%3Fcode%3D{code}")).into_response()
}

async fn device_page(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<DeviceQuery>,
) -> Response {
    if current_user(&state, &headers).await.is_none() {
        return device_login_redirect(&query.code);
    }
    let code = query.code.trim().to_uppercase();
    let step = if code.is_empty() {
        DeviceState::Prompt
    } else {
        match crate::device::lookup(&state.0.app.pool, &code).await {
            Ok(Some(request)) => DeviceState::Confirm(request.label),
            _ => DeviceState::Unknown,
        }
    };
    Html(
        DeviceT { state: step, code }
            .render()
            .expect("device renders"),
    )
    .into_response()
}

#[derive(Deserialize)]
struct DeviceDecision {
    user_code: String,
    decision: String,
}

async fn device_decide(
    State(state): State<WebState>,
    headers: HeaderMap,
    Form(form): Form<DeviceDecision>,
) -> Response {
    let code = form.user_code.trim().to_uppercase();
    let Some(user) = current_user(&state, &headers).await else {
        return device_login_redirect(&code);
    };
    // Only the confirmation screen posts a decision: entering a code is a GET
    // that shows what is being asked for, so nobody grants access by reflex.
    let approve = form.decision == "approve";
    let step = match crate::device::decide(&state.0.app.pool, &code, user.id, approve).await {
        Ok(true) if approve => DeviceState::Approved,
        Ok(true) => DeviceState::Denied,
        _ => DeviceState::Unknown,
    };
    Html(
        DeviceT { state: step, code }
            .render()
            .expect("device renders"),
    )
    .into_response()
}

// ------------------------------------------------------------------- templates

/// Which step of connecting a device the human is looking at.
pub enum DeviceState {
    Prompt,
    Confirm(String),
    Approved,
    Denied,
    Unknown,
}

#[derive(Template)]
#[template(path = "device.html")]
struct DeviceT {
    state: DeviceState,
    code: String,
}

#[derive(Template)]
#[template(path = "landing.html")]
struct LandingT;

/// The public documentation shell: sidebar plus one pre-authored page.
/// Content lives in templates/docs/*.html as plain fragments compiled into
/// the binary — the docs deploy with the code they describe.
#[derive(Template)]
#[template(path = "docs.html")]
struct DocsT {
    title: &'static str,
    active: String,
    pages: Vec<(&'static str, &'static str)>,
    inner: &'static str,
    description: &'static str,
    canonical: String,
}

struct DocsPage {
    slug: &'static str,
    label: &'static str,
    title: &'static str,
    /// Meta description — what a search result shows under the title.
    description: &'static str,
    content: &'static str,
}

/// Sidebar order.
const DOCS_PAGES: [DocsPage; 10] = [
    DocsPage {
        slug: "getting-started",
        label: "Getting started",
        title: "Getting started",
        description: "Install the rusted CLI, sign in, deploy your first JavaScript function, and call it over HTTPS — from nothing to a live endpoint in about two minutes.",
        content: include_str!("../templates/docs/getting_started.html"),
    },
    DocsPage {
        slug: "cli",
        label: "CLI reference",
        title: "CLI reference",
        description: "Every rusted CLI command — create, run, push, logs, types, inbox — the full local development loop for deployed JavaScript functions.",
        content: include_str!("../templates/docs/cli.html"),
    },
    DocsPage {
        slug: "module",
        label: "Module reference",
        title: "Module reference",
        description: "Every field a rusted function can declare: the http, mcp, and app surface exports (name, methods, path, access, tools, routes, auth) and the config export (secrets, state, object storage).",
        content: include_str!("../templates/docs/module.html"),
    },
    DocsPage {
        slug: "runtime",
        label: "Runtime reference",
        title: "Runtime reference",
        description: "Every global and context helper inside the rusted sandbox — fetch, URL, TextEncoder, native crypto and codecs, declared capabilities — and what is deliberately absent.",
        content: include_str!("../templates/docs/runtime.html"),
    },
    DocsPage {
        slug: "mcp",
        label: "MCP",
        title: "MCP",
        description: "Serve Model Context Protocol tools from a deployed function: schema-validated tools, optional OAuth protection, and per-environment audiences.",
        content: include_str!("../templates/docs/mcp.html"),
    },
    DocsPage {
        slug: "apps",
        label: "Web apps",
        title: "Web apps",
        description: "Express-style routes, middleware, and path parameters under one function's URL — with the database and HTML fragments, one pushed file is a complete interactive web app.",
        content: include_str!("../templates/docs/apps.html"),
    },
    DocsPage {
        slug: "database",
        label: "Database",
        title: "Database",
        description: "A real SQL database per account and environment — SQLite in-process, shared across your functions, with parameterized queries, atomic transactions, and a console table browser.",
        content: include_str!("../templates/docs/database.html"),
    },
    DocsPage {
        slug: "security",
        label: "Security",
        title: "Security",
        description: "How rusted isolates untrusted code: the QuickJS sandbox, API keys, encrypted secrets, environments, sealed values, and public functions.",
        content: include_str!("../templates/docs/security.html"),
    },
    DocsPage {
        slug: "inbox",
        label: "Inboxes",
        title: "Inboxes",
        description: "Inboxes give CLIs and AI agents an inbound HTTPS address: a throwaway URL that accepts POSTs from anyone and holds them until you read them.",
        content: include_str!("../templates/docs/inbox.html"),
    },
    DocsPage {
        slug: "ai-agents",
        label: "AI agents",
        title: "AI agents",
        description: "rusted is the MCP server that lets AI agents write their own tools — create a live endpoint in one call, invoke it in milliseconds, delete it when done.",
        content: include_str!("../templates/docs/ai_agents.html"),
    },
];

async fn docs_index() -> Response {
    Redirect::to("/docs/getting-started").into_response()
}

async fn docs_page(State(state): State<WebState>, Path(page): Path<String>) -> Response {
    let Some(found) = DOCS_PAGES.iter().find(|entry| entry.slug == page.as_str()) else {
        return Redirect::to("/docs/getting-started").into_response();
    };
    Html(
        DocsT {
            title: found.title,
            active: found.slug.to_string(),
            pages: DOCS_PAGES
                .iter()
                .map(|entry| (entry.slug, entry.label))
                .collect(),
            inner: found.content,
            description: found.description,
            canonical: state.0.app.console_url(&format!("/docs/{}", found.slug)),
        }
        .render()
        .expect("docs render"),
    )
    .into_response()
}

/// Plain-text signposting for crawlers. The console is auth-gated anyway;
/// disallowing it just keeps error pages out of indexes.
async fn robots_txt(State(state): State<WebState>) -> Response {
    let body = format!(
        "User-agent: *\nAllow: /\nDisallow: /console\nDisallow: /login\nDisallow: /device\n\nSitemap: {}\n",
        state.0.app.console_url("/sitemap.xml")
    );
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// The llms.txt convention (llmstxt.org): a curated markdown summary an LLM
/// reads in one fetch instead of crawling HTML. For a platform whose pitch is
/// "point your agent here", this file is the front door.
fn llms_preamble(state: &WebState) -> String {
    let origin = |path: &str| state.0.app.console_url(path);
    format!(
        "# rusted\n\n\
         > rusted turns a JavaScript function into a live HTTPS endpoint in under a second — \
         for humans from the CLI, for AI agents over MCP. Every call runs in a fresh QuickJS \
         sandbox that boots in about a millisecond, with hard wall-clock, heap, and output \
         limits, no filesystem, and SSRF-guarded fetch.\n\n\
         Connect an agent: MCP endpoint {} (POST JSON-RPC, Authorization: Bearer <rusted API key>). \
         Hosted assistants can add the same URL as a connector and sign in with OAuth instead — \
         discovery, dynamic client registration, and PKCE are supported.\n\
         The whole API is six tools: execute, deploy, list, delete, inbox_create, inbox_read.\n\
         Deployed functions are plain HTTP: POST {} — per environment: {}.\n",
        origin("/mcp"),
        origin("/f/<name>"),
        origin("/f/@<env>/<name>"),
    )
}

async fn llms_txt(State(state): State<WebState>) -> Response {
    let origin = |path: &str| state.0.app.console_url(path);
    let mut body = llms_preamble(&state);
    body.push_str("\n## Docs\n\n");
    for entry in &DOCS_PAGES {
        body.push_str(&format!(
            "- [{}]({}): {}\n",
            entry.title,
            origin(&format!("/docs/{}", entry.slug)),
            entry.description
        ));
    }
    body.push_str(&format!(
        "\n## Optional\n\n- [llms-full.txt]({}): the entire documentation as plain text in one fetch\n",
        origin("/llms-full.txt")
    ));
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

async fn llms_full_txt(State(state): State<WebState>) -> Response {
    let mut body = llms_preamble(&state);
    for entry in &DOCS_PAGES {
        body.push_str("\n---\n");
        body.push_str(&html_to_text(entry.content));
    }
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// Good-enough markdown-ish rendering of the docs fragments: headings, list
/// markers, and fenced code blocks survive; everything else is stripped. The
/// fragments are hand-authored HTML with no scripts or styles, so a tag
/// stripper is all this needs to be.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            rest = "";
            break;
        };
        let tag = &after[..gt];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        match (name.as_str(), closing) {
            ("h1", false) => out.push_str("\n\n# "),
            ("h2", false) => out.push_str("\n\n## "),
            ("h3", false) => out.push_str("\n\n### "),
            ("p" | "div" | "ul" | "ol" | "table", false) => out.push('\n'),
            ("li", false) => out.push_str("\n- "),
            ("pre", false) => out.push_str("\n\n```\n"),
            ("pre", true) => out.push_str("\n```\n"),
            ("br", false) => out.push('\n'),
            ("td" | "th", true) => out.push_str("  "),
            ("tr", true) => out.push('\n'),
            _ => {}
        }
        rest = &after[gt + 1..];
    }
    out.push_str(rest);
    let decoded = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    // Source indentation and dropped tags leave ragged blank runs; two
    // newlines is the most markdown ever needs.
    let mut cleaned = String::with_capacity(decoded.len());
    let mut blank_run = 0usize;
    for line in decoded.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    cleaned.trim_start().to_string()
}

async fn sitemap_xml(State(state): State<WebState>) -> Response {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    body.push_str(&format!(
        "  <url><loc>{}</loc></url>\n",
        state.0.app.console_url("/")
    ));
    for entry in &DOCS_PAGES {
        body.push_str(&format!(
            "  <url><loc>{}</loc></url>\n",
            state.0.app.console_url(&format!("/docs/{}", entry.slug))
        ));
    }
    body.push_str("</urlset>\n");
    ([(header::CONTENT_TYPE, "application/xml")], body).into_response()
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginT {
    configured: bool,
    google_configured: bool,
    callback_url: String,
}

#[derive(Template)]
#[template(path = "authorize.html")]
struct AuthorizeT {
    client_name: String,
    login: String,
    redirect_host: String,
    /// Carried through the consent form so the POST can be re-validated from
    /// scratch rather than trusting anything held server-side between requests.
    fields: Vec<(String, String)>,
}

#[derive(Template)]
#[template(path = "console.html")]
struct ConsoleT {
    active: String,
    lambdas: Vec<NavFn>,
    user_name: String,
    user_initial: String,
    is_admin: bool,
    inner: String,
}

pub struct Stats {
    invocations: String,
    invocations_delta: String,
    p95_exec: String,
    error_rate: String,
    errors: String,
}

pub struct Bar {
    label: String,
    value: u32,
    pct: u32,
    peak: bool,
}

pub struct Recent {
    function: String,
    outcome: String,
    /// "200", "403", or empty for tool calls and pre-status rows.
    status: String,
    ok: bool,
    /// The failure message, revealed by the row's indicator.
    detail: Option<String>,
    wall: String,
    cpu: String,
    exec: String,
    when: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardT {
    window: String,
    stats: Stats,
    bars: Vec<Bar>,
    functions: Vec<String>,
    filter_function: String,
    filter_errors: bool,
    /// Pre-rendered rows, so the filters can swap just this fragment.
    invocations: String,
}

#[derive(Template)]
#[template(path = "invocations.html")]
struct InvocationsT {
    rows: Vec<Recent>,
    page: i64,
    prev_url: Option<String>,
    next_url: Option<String>,
}

/// Rows per page in the console.
const PAGE_SIZE: i64 = 20;

pub struct KeyView {
    id: Uuid,
    name: String,
    masked: String,
    created: String,
    last_used: String,
}

#[derive(Template)]
#[template(path = "keys.html")]
struct KeysT {
    keys: Vec<KeyView>,
}

#[derive(Template)]
#[template(path = "key_created.html")]
struct KeyCreatedT {
    name: String,
    token: String,
}

pub struct SecretRow {
    name: String,
    created: String,
    updated: String,
}

#[derive(Template)]
#[template(path = "secrets.html")]
struct SecretsT {
    /// Whether this server holds a master key at all; off, the page explains
    /// what to configure instead of offering a form that would be refused.
    enabled: bool,
    /// Why the last set/delete was refused, shown above the form.
    error: Option<String>,
    rows: Vec<SecretRow>,
    /// Every environment this account can resolve, prod first.
    envs: Vec<String>,
    /// The environment the page is showing.
    env: String,
}

pub struct PlanCard {
    code: String,
    /// Whether this card offers a working button.
    selectable: bool,
    name: String,
    version: i32,
    price: String,
    price_cents: i32,
    period: String,
    current: bool,
    max_functions: String,
    max_script: String,
    exec: String,
    rate: String,
    outbound: String,
    analytics: String,
}

#[derive(Template)]
#[template(path = "billing.html")]
struct BillingT {
    current_name: String,
    plans: Vec<PlanCard>,
    /// Whether this person may switch plans at all. Off, the cards still show
    /// what each tier offers — that is the point of a pricing page — but say
    /// so instead of offering a button that would be refused.
    can_switch: bool,
}

#[derive(Template)]
#[template(path = "checkout.html")]
struct CheckoutT {
    code: String,
    name: String,
    version: i32,
    price: String,
    price_cents: i32,
    period: String,
    billed: String,
    email: String,
    cardholder: String,
}

/// One tool of an mcp function, ready to render: schema pretty-printed.
struct ToolRow {
    name: String,
    description: String,
    schema_json: String,
}

/// One environment's invocation URL for the header.
struct EnvUrl {
    env: String,
    url: String,
}

#[derive(Template)]
#[template(path = "function.html")]
struct FunctionT {
    name: String,
    published: bool,
    methods: Vec<String>,
    url_example: String,
    /// Every environment's URL, prod first.
    env_urls: Vec<EnvUrl>,
    /// Absolute deploy time of the serving revision, e.g. "2026-08-14 19:52 UTC".
    deployed_at: String,
    /// Relative form, e.g. "2 h ago".
    deployed_ago: String,
    revision: u64,
    size: String,
    hidden_kb: usize,
    code_json: String,
    /// Data-plane protocol: `"http"` or `"mcp"`.
    kind: String,
    /// Which surface pushed the serving revision: "CLI", "web editor", "agent".
    via: String,
    /// Who may call it on THIS server: "public", "private", or "key required"
    /// (undeclared on a --require-auth server).
    access: &'static str,
    /// Empty for http functions.
    tools: Vec<ToolRow>,
    mcp_public: bool,
    /// Paste-ready MCP client config block; empty for http functions.
    client_config: String,
}

#[derive(Template)]
#[template(path = "test_result.html")]
struct TestResultT {
    ok: bool,
    status: String,
    took_ms: String,
    content_type: String,
    body: String,
}

// ------------------------------------------------------------------- pages

async fn landing() -> Html<String> {
    Html(LandingT.render().expect("landing renders"))
}

#[derive(Deserialize)]
struct LoginQuery {
    #[serde(default)]
    next: Option<String>,
}

async fn login(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    // Already signed in: the form would only look like a failed login.
    if current_user(&state, &headers).await.is_some() {
        return Redirect::to("/console").into_response();
    }
    let callback_url = state
        .0
        .oauth
        .as_ref()
        .map(|o| o.callback_url.clone())
        .unwrap_or_else(|| "http://127.0.0.1:7412/auth/github/callback".to_string());
    // Remembered across the round trip to GitHub so sign-in returns the human
    // to whatever they were doing.
    let cookie = match query.next.as_deref().filter(|n| n.starts_with('/')) {
        Some(next) => {
            format!("rusted_after_login={next}; Path=/; HttpOnly; Max-Age=600; SameSite=Lax")
        }
        None => "rusted_after_login=; Path=/; Max-Age=0".to_string(),
    };
    (
        [(SET_COOKIE, cookie)],
        Html(
            LoginT {
                configured: state.0.oauth.is_some(),
                google_configured: state.0.google.is_some(),
                callback_url,
            }
            .render()
            .expect("login renders"),
        ),
    )
        .into_response()
}

// ------------------------------------------------------------ oauth consent

/// These two pages report a client's mistake back to a human, and the text
/// includes values the client chose — so it is escaped rather than trusted.
fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The parameters a client sent, flattened for the consent form so the POST
/// can be validated on its own terms.
fn authorize_fields(params: &crate::oauth::AuthorizeParams) -> Vec<(String, String)> {
    let mut fields = vec![
        ("client_id".into(), params.client_id.clone()),
        ("redirect_uri".into(), params.redirect_uri.clone()),
        ("response_type".into(), params.response_type.clone()),
        ("code_challenge".into(), params.code_challenge.clone()),
        (
            "code_challenge_method".into(),
            params.code_challenge_method.clone(),
        ),
    ];
    if let Some(v) = &params.state {
        fields.push(("state".into(), v.clone()));
    }
    if let Some(v) = &params.resource {
        fields.push(("resource".into(), v.clone()));
    }
    if let Some(v) = &params.scope {
        fields.push(("scope".into(), v.clone()));
    }
    fields
}

fn host_of(uri: &str) -> String {
    uri.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(uri)
        .to_string()
}

/// Shows a human what a client is asking for.
///
/// Signed out, this sends them through GitHub and back here with the request
/// intact, so approving does not mean starting over.
async fn oauth_authorize(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(params): Query<crate::oauth::AuthorizeParams>,
    raw: RawQuery,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Some(user) => user,
        None => {
            let back = format!("/oauth/authorize?{}", raw.0.unwrap_or_default());
            return Redirect::to(&format!("/login?next={}", urlencoding::encode(&back)))
                .into_response();
        }
    };

    // Checked before anyone is asked to approve: a bad request should read as
    // an error, not as a consent screen for something that cannot work.
    let pending = match crate::oauth::validate_authorize(&state.0.app, &params).await {
        Ok(pending) => pending,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Html(format!(
                    "<p style=\"font:14px system-ui;padding:2rem;color:#eee;background:#1d0e07\">\
                     This authorization request cannot be completed: {}</p>",
                    escape_html(&message)
                )),
            )
                .into_response();
        }
    };

    Html(
        AuthorizeT {
            client_name: pending.client_name,
            login: user.login.clone(),
            redirect_host: host_of(&pending.redirect_uri),
            fields: authorize_fields(&params),
        }
        .render()
        .expect("authorize renders"),
    )
    .into_response()
}

#[derive(Deserialize)]
struct AuthorizeDecision {
    decision: String,
    #[serde(flatten)]
    params: crate::oauth::AuthorizeParams,
}

/// Records the decision. Everything is re-validated: the form came back from a
/// browser, and so did the parameters on it.
async fn oauth_authorize_decide(
    State(state): State<WebState>,
    headers: HeaderMap,
    Form(body): Form<AuthorizeDecision>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };

    if body.decision != "approve" {
        // Refusal is an answer the client is entitled to, delivered the way the
        // spec expects rather than as a dead end in the browser.
        let sep = if body.params.redirect_uri.contains('?') {
            "&"
        } else {
            "?"
        };
        let mut location = format!(
            "{}{sep}error=access_denied&error_description={}",
            body.params.redirect_uri,
            urlencoding::encode("the person declined this request")
        );
        if let Some(client_state) = &body.params.state {
            location.push_str(&format!("&state={}", urlencoding::encode(client_state)));
        }
        return Redirect::to(&location).into_response();
    }

    match crate::oauth::grant(&state.0.app, user.id, &body.params).await {
        Ok(location) => Redirect::to(&location).into_response(),
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Html(format!(
                "<p style=\"font:14px system-ui;padding:2rem;color:#eee;background:#1d0e07\">\
                 Could not complete authorization: {}</p>",
                escape_html(&message)
            )),
        )
            .into_response(),
    }
}

/// Resolves the signed-in user or produces the redirect to /login.
async fn require_user(state: &WebState, headers: &HeaderMap) -> Result<User, Response> {
    match current_user(state, headers).await {
        Some(user) => Ok(user),
        None => Err(Redirect::to("/login").into_response()),
    }
}

/// Renders a console page: the bare partial for htmx navigation, or the full
/// shell (sidebar + partial) for direct visits and reloads.
async fn console_page(
    state: &WebState,
    headers: &HeaderMap,
    user: &User,
    active: &str,
    inner: String,
) -> Response {
    if headers.contains_key("hx-request") {
        return Html(inner).into_response();
    }
    let lambdas = nav_rows(state, user.id).await;
    let display = user.name.clone().unwrap_or_else(|| user.login.clone());
    let shell = ConsoleT {
        active: active.to_string(),
        lambdas,
        user_initial: display
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string(),
        user_name: display,
        is_admin: user.admin,
        inner,
    };
    Html(shell.render().expect("console renders")).into_response()
}

fn human_count(n: i64) -> String {
    let text = n.to_string();
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if i > 0 && (text.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Just the rows, so the filter form can swap them without a page load.
async fn invocations_inner(
    state: &WebState,
    user: &User,
    function: &str,
    errors_only: bool,
    page: i64,
) -> String {
    let page = page.max(1);
    // One extra row answers "is there a next page?" without a count query.
    let mut fetched = crate::analytics::recent(
        &state.0.app.pool,
        user.id,
        PAGE_SIZE + 1,
        (page - 1) * PAGE_SIZE,
        Some(function).filter(|f| !f.is_empty()),
        errors_only,
    )
    .await;
    let has_next = fetched.len() as i64 > PAGE_SIZE;
    fetched.truncate(PAGE_SIZE as usize);
    let link = |page: i64| {
        let mut url = format!("/console/invocations?page={page}");
        if !function.is_empty() {
            url.push_str(&format!("&function={function}"));
        }
        if errors_only {
            url.push_str("&errors=1");
        }
        url
    };
    let rows = fetched
        .iter()
        .map(|row| Recent {
            function: row.function.clone(),
            outcome: row.outcome.clone(),
            status: row.status.map(|s| s.to_string()).unwrap_or_default(),
            // Success means what the caller saw: a 4xx/5xx answer is not ok.
            ok: row.outcome == "success" && row.status.unwrap_or(200) < 400,
            detail: row.detail.clone(),
            wall: format!("{:.2} ms", row.wall_ms),
            cpu: format!("{:.2} ms", row.cpu_ms),
            exec: format!("{:.2} ms", row.exec_ms),
            when: ago(row.at),
        })
        .collect();
    InvocationsT {
        rows,
        page,
        prev_url: (page > 1).then(|| link(page - 1)),
        next_url: has_next.then(|| link(page + 1)),
    }
    .render()
    .expect("invocations render")
}

async fn dashboard_inner(
    state: &WebState,
    user: &User,
    filter_function: &str,
    filter_errors: bool,
    page: i64,
) -> String {
    let app = &state.0.app;
    let plan = crate::plans::effective_plan(&app.pool, &app.plan_cache, Some(user.id)).await;
    let days = plan.limits.analytics_days.clamp(1, 30);
    // Headline tiles come from the in-process OpenTelemetry pipeline
    // (lifetime totals, persisted across restarts); the day chart and the
    // invocation rows below stay event-based from Postgres.
    let names = app.store.names_for_user(user.id).await.unwrap_or_default();
    let overall = app.telemetry.overall(Some(&names));
    let buckets = crate::analytics::per_day(&app.pool, user.id, days).await;
    let max = buckets.iter().map(|b| b.value).max().unwrap_or(0).max(1);
    let bars = buckets
        .iter()
        .map(|b| Bar {
            label: b.label.clone(),
            value: b.value as u32,
            pct: ((b.value * 100 / max) as u32).max(2),
            peak: b.value == max && b.value > 0,
        })
        .collect();
    let invocations = invocations_inner(state, user, filter_function, filter_errors, page).await;
    let functions = crate::analytics::invoked_functions(&app.pool, user.id).await;
    let error_rate = if overall.invocations == 0 {
        "0%".to_string()
    } else {
        format!("{:.1}%", overall.error_rate * 100.0)
    };
    DashboardT {
        window: format!("all-time · opentelemetry · chart: last {days} days"),
        stats: Stats {
            invocations: human_count(overall.invocations as i64),
            // Cumulative counters carry no prior-window to compare against.
            invocations_delta: "—".to_string(),
            p95_exec: overall
                .p95_exec_ms
                .map(|p| format!("{p:.1} ms"))
                .unwrap_or_else(|| "—".to_string()),
            error_rate,
            errors: human_count(overall.errors as i64),
        },
        bars,
        functions,
        filter_function: filter_function.to_string(),
        filter_errors,
        invocations,
    }
    .render()
    .expect("dashboard renders")
}

async fn console_home(State(state): State<WebState>, headers: HeaderMap) -> Response {
    match require_user(&state, &headers).await {
        Ok(user) => {
            let inner = dashboard_inner(&state, &user, "", false, 1).await;
            console_page(&state, &headers, &user, "dashboard", inner).await
        }
        Err(redirect) => redirect,
    }
}

#[derive(Deserialize, Default)]
struct InvocationFilter {
    #[serde(default)]
    function: String,
    #[serde(default)]
    errors: Option<String>,
    #[serde(default)]
    page: Option<i64>,
}

async fn page_invocations(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(filter): Query<InvocationFilter>,
) -> Response {
    match require_user(&state, &headers).await {
        Ok(user) => Html(
            invocations_inner(
                &state,
                &user,
                &filter.function,
                filter.errors.is_some(),
                filter.page.unwrap_or(1),
            )
            .await,
        )
        .into_response(),
        Err(redirect) => redirect,
    }
}

async fn page_dashboard(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(filter): Query<InvocationFilter>,
) -> Response {
    match require_user(&state, &headers).await {
        Ok(user) => {
            let inner = dashboard_inner(
                &state,
                &user,
                &filter.function,
                filter.errors.is_some(),
                filter.page.unwrap_or(1),
            )
            .await;
            console_page(&state, &headers, &user, "dashboard", inner).await
        }
        Err(redirect) => redirect,
    }
}

// ------------------------------------------------------------------- keys

/// Human names for revision origins.
fn via_label(via: &str) -> &'static str {
    match via {
        "editor" => "web editor",
        "agent" => "agent",
        "" => "",
        _ => "CLI",
    }
}

fn ago(epoch: i64) -> String {
    let delta = (now_epoch() as i64 - epoch).max(0);
    match delta {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", delta / 60),
        3600..=86399 => format!("{} h ago", delta / 3600),
        _ => format!("{} d ago", delta / 86400),
    }
}

async fn keys_inner(state: &WebState, user: &User) -> String {
    let rows = sqlx::query(
        "SELECT id, name, prefix, lookup,
                extract(epoch FROM created_at)::bigint AS created,
                extract(epoch FROM last_used_at)::bigint AS last_used
         FROM api_keys
         WHERE user_id = $1 AND revoked_at IS NULL
         ORDER BY id DESC",
    )
    .bind(user.id)
    .fetch_all(&state.0.app.pool)
    .await
    .unwrap_or_default();
    let keys = rows
        .iter()
        .map(|row| KeyView {
            id: row.get("id"),
            name: row.get("name"),
            masked: format!(
                "rk_live_{}_{}…",
                row.get::<String, _>("lookup"),
                row.get::<String, _>("prefix")
            ),
            created: ago(row.get("created")),
            last_used: row
                .get::<Option<i64>, _>("last_used")
                .map(|at| format!("last used {}", ago(at)))
                .unwrap_or_else(|| "never used".to_string()),
        })
        .collect();
    KeysT { keys }.render().expect("keys renders")
}

async fn page_keys(State(state): State<WebState>, headers: HeaderMap) -> Response {
    match require_user(&state, &headers).await {
        Ok(user) => {
            let inner = keys_inner(&state, &user).await;
            console_page(&state, &headers, &user, "keys", inner).await
        }
        Err(redirect) => redirect,
    }
}

async fn key_create(State(state): State<WebState>, headers: HeaderMap) -> Response {
    const NAMES: [&str; 6] = [
        "swift-falcon",
        "quiet-ember",
        "amber-fox",
        "late-comet",
        "dry-sage",
        "warm-static",
    ];
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let nonce = now_epoch() as usize;
    let name = format!("key-{}", NAMES[nonce % NAMES.len()]);
    match auth::create_key(&state.0.app.pool, user.id, &name).await {
        Ok((_, token)) => {
            Html(KeyCreatedT { name, token }.render().expect("key renders")).into_response()
        }
        Err(e) => Html(format!(
            r#"<div class="rounded-xl border border-blood/40 bg-rust-950 px-6 py-4 font-mono text-xs text-blood">creating the key failed: {e}</div>"#
        ))
        .into_response(),
    }
}

async fn key_revoke(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let _ = auth::revoke_key(&state.0.app.pool, &state.0.app.auth, user.id, id).await;
    Html(String::new()).into_response()
}

// ------------------------------------------------------------------- secrets

/// The whole page fragment: set and delete re-render it, so the list, the
/// form, the env tabs, and any error always agree.
async fn secrets_inner(state: &WebState, user: &User, env: &str, error: Option<String>) -> String {
    let app = &state.0.app;
    let envs = crate::secrets::list_envs(&app.pool, user.id).await;
    // An unknown env in the URL falls back to prod rather than 404ing a page
    // whose tabs are the way to navigate envs.
    let env = if envs.iter().any(|e| e == env) {
        env.to_string()
    } else {
        crate::secrets::PROD_ENV.to_string()
    };
    let store = &app.secrets;
    let rows = store
        .list(user.id, &env)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|meta| SecretRow {
            name: meta.name,
            created: ago(meta.created_at as i64),
            updated: ago(meta.updated_at as i64),
        })
        .collect();
    SecretsT {
        enabled: store.enabled(),
        error,
        rows,
        envs,
        env,
    }
    .render()
    .expect("secrets renders")
}

#[derive(Deserialize, Default)]
struct EnvQuery {
    #[serde(default)]
    env: Option<String>,
}

async fn page_secrets(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<EnvQuery>,
) -> Response {
    match require_user(&state, &headers).await {
        Ok(user) => {
            let env = query.env.unwrap_or_else(|| crate::secrets::PROD_ENV.into());
            let inner = secrets_inner(&state, &user, &env, None).await;
            console_page(&state, &headers, &user, "secrets", inner).await
        }
        Err(redirect) => redirect,
    }
}

#[derive(Deserialize)]
struct SecretForm {
    #[serde(default)]
    env: Option<String>,
    name: String,
    value: String,
}

async fn secret_set(
    State(state): State<WebState>,
    headers: HeaderMap,
    Form(form): Form<SecretForm>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let env = form.env.unwrap_or_else(|| crate::secrets::PROD_ENV.into());
    // Trimmed because paste brings whitespace along; a credential that
    // genuinely needs surrounding whitespace does not exist.
    let error = state
        .0
        .app
        .secrets
        .set(user.id, &env, form.name.trim(), form.value.trim())
        .await
        .err();
    Html(secrets_inner(&state, &user, &env, error).await).into_response()
}

async fn secret_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path((env, name)): Path<(String, String)>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let error = state.0.app.secrets.delete(user.id, &env, &name).await.err();
    Html(secrets_inner(&state, &user, &env, error).await).into_response()
}

#[derive(Deserialize)]
struct EnvironmentForm {
    name: String,
}

async fn environment_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Form(form): Form<EnvironmentForm>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let name = form.name.trim().to_string();
    match crate::secrets::create_env(&state.0.app.pool, user.id, &name).await {
        Ok(()) => Html(secrets_inner(&state, &user, &name, None).await).into_response(),
        Err(error) => {
            Html(secrets_inner(&state, &user, crate::secrets::PROD_ENV, Some(error)).await)
                .into_response()
        }
    }
}

async fn environment_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let error = crate::secrets::delete_env(&state.0.app.pool, user.id, &name)
        .await
        .err();
    Html(secrets_inner(&state, &user, crate::secrets::PROD_ENV, error).await).into_response()
}

// ------------------------------------------------------------------- billing

fn money(cents: i32) -> String {
    if cents == 0 {
        "Free".to_string()
    } else if cents % 100 == 0 {
        format!("${}", cents / 100)
    } else {
        format!("${}.{:02}", cents / 100, cents % 100)
    }
}

fn human_bytes(bytes: i64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MB", bytes / (1024 * 1024))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn human_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{} s", ms / 1000)
    } else {
        format!("{ms} ms")
    }
}

fn card(plan: &crate::plans::Plan, current: bool, can_switch: bool) -> PlanCard {
    let l = &plan.limits;
    PlanCard {
        code: plan.code.clone(),
        name: plan.name.clone(),
        version: plan.version,
        price: money(plan.price_cents),
        price_cents: plan.price_cents,
        period: if plan.price_cents == 0 {
            String::new()
        } else {
            "/mo".to_string()
        },
        current,
        // A free tier is the default anyway, so switching to it changes
        // nothing anyone would pay for; the paid ones are what need gating.
        selectable: can_switch || plan.price_cents == 0,
        max_functions: l.max_functions.to_string(),
        max_script: human_bytes(l.max_script_bytes),
        exec: human_ms(l.exec_ms),
        rate: if l.rate_per_min % 60 == 0 && l.rate_per_min > 60 {
            format!("{}/sec", l.rate_per_min / 60)
        } else {
            format!("{}/min", l.rate_per_min)
        },
        outbound: if l.outbound_reqs == 0 {
            "none".to_string()
        } else {
            format!("{} per run", l.outbound_reqs)
        },
        analytics: format!("{} days", l.analytics_days),
    }
}

async fn page_billing(State(state): State<WebState>, headers: HeaderMap) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let app = &state.0.app;
    let current = crate::plans::effective_plan(&app.pool, &app.plan_cache, Some(user.id)).await;
    let catalog = crate::plans::catalog(&app.pool).await.unwrap_or_default();
    let can_switch = crate::plans::may_change_plan(&user.login);
    let inner = BillingT {
        current_name: current.name.clone(),
        plans: catalog
            .iter()
            .map(|plan| card(plan, plan.id == current.id, can_switch))
            .collect(),
        can_switch,
    }
    .render()
    .expect("billing renders");
    console_page(&state, &headers, &user, "billing", inner).await
}

/// Whether this person may select this plan.
///
/// The code arrives in the URL, so an internal plan must not be reachable by
/// guessing its name, and a hidden button is not a control — checkout takes no
/// payment, so without the allowlist any signed-in account could hand itself
/// the top tier by POSTing the path directly.
fn may_select(login: &str, code: &str) -> bool {
    crate::plans::is_public(code) && crate::plans::may_change_plan(login)
}

async fn page_checkout(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    if !may_select(&user.login, &code) {
        return Redirect::to("/console/billing").into_response();
    }
    let Ok(Some(plan)) = crate::plans::latest_by_code(&state.0.app.pool, &code).await else {
        return Redirect::to("/console/billing").into_response();
    };
    Html(
        CheckoutT {
            code: plan.code.clone(),
            name: plan.name.clone(),
            version: plan.version,
            price: money(plan.price_cents),
            price_cents: plan.price_cents,
            period: if plan.price_cents == 0 {
                String::new()
            } else {
                "/mo".to_string()
            },
            billed: if plan.price_cents == 0 {
                "never".to_string()
            } else {
                "monthly".to_string()
            },
            email: format!("{}@users.noreply.github.com", user.login),
            cardholder: user.name.clone().unwrap_or_else(|| user.login.clone()),
        }
        .render()
        .expect("checkout renders"),
    )
    .into_response()
}

async fn confirm_checkout(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    if !may_select(&user.login, &code) {
        return Redirect::to("/console/billing").into_response();
    }
    let Ok(Some(plan)) = crate::plans::latest_by_code(&state.0.app.pool, &code).await else {
        return Redirect::to("/console/billing").into_response();
    };
    let _ = crate::plans::subscribe(&state.0.app.pool, user.id, plan.id).await;
    Redirect::to("/console/billing").into_response()
}

// ------------------------------------------------------------------- lambda view

/// Bundlers label each module in the output and place the entry last, so the
/// developer's own code is the tail. Handles both markers we're likely to see:
/// rolldown's `//#region <path>` and esbuild's `// <path>`. Everything before
/// the first non-dependency label is somebody else's code.
fn split_user_code(source: &str) -> (&str, usize) {
    let mut entry_start = None;
    let mut in_deps = false;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let label = trimmed
            .strip_prefix("//#region ")
            .or_else(|| trimmed.strip_prefix("// "));
        if let Some(path) = label {
            // `\0`-prefixed labels are the bundler's own injected helpers.
            // `http:`/`https:` labels are the console editor's esm.sh
            // modules — npm dependencies fetched at bundle time.
            if path.starts_with("node_modules/")
                || path.starts_with('\0')
                || path.starts_with("http:")
                || path.starts_with("https:")
            {
                in_deps = true;
                entry_start = None;
            } else if in_deps && entry_start.is_none() {
                entry_start = Some(offset);
            }
        }
        offset += line.len();
    }
    match entry_start {
        Some(at) => (&source[at..], at),
        None => (source, 0),
    }
}

fn pretty_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

async fn page_function(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let Ok(Some(hit)) = state.0.app.store.fetch(&name).await else {
        return console_page(&state, &headers, &user, "", missing_lambda(&name)).await;
    };
    let source = &hit.source;
    let trigger = hit.trigger.clone();
    let route_suffix = trigger.path.as_deref().unwrap_or("").to_string();
    let route = format!("/f/{name}{route_suffix}");
    let url = state.0.app.data_url(&route);
    let env_urls: Vec<EnvUrl> = crate::secrets::list_envs(&state.0.app.pool, user.id)
        .await
        .into_iter()
        .map(|env| {
            let path = if env == crate::secrets::PROD_ENV {
                format!("/f/{name}{route_suffix}")
            } else {
                format!("/f/@{env}/{name}{route_suffix}")
            };
            EnvUrl {
                url: state.0.app.data_url(&path),
                env,
            }
        })
        .collect();
    // When the serving revision went live — the store's cached view carries
    // no timestamps, so one small query pays for the header line.
    let (deployed_at, deployed_ago, via) = sqlx::query(
        "SELECT to_char(r.created_at, 'YYYY-MM-DD HH24:MI \"UTC\"') AS at_text,
                extract(epoch FROM r.created_at)::bigint AS at, r.via
         FROM revisions r WHERE r.function_name = $1 AND r.rev = $2",
    )
    .bind(&name)
    .bind(hit.rev as i64)
    .fetch_optional(&state.0.app.pool)
    .await
    .ok()
    .flatten()
    .map(|row| {
        (
            row.get::<String, _>("at_text"),
            ago(row.get::<i64, _>("at")),
            via_label(row.get::<String, _>("via").as_str()).to_string(),
        )
    })
    .unwrap_or_else(|| ("unknown".to_string(), String::new(), String::new()));
    let (user_code, hidden) = split_user_code(source);
    let code_json = serde_json::json!({ "user": user_code, "full": source })
        .to_string()
        .replace("</", "<\\/");
    let meta = hit.mcp.clone().unwrap_or(serde_json::Value::Null);
    let tools: Vec<ToolRow> = meta["tools"]
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(tool, spec)| ToolRow {
                    name: tool.clone(),
                    description: spec["description"].as_str().unwrap_or("").to_string(),
                    schema_json: serde_json::to_string_pretty(&spec["inputSchema"])
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mcp_public = meta["public"].as_bool().unwrap_or(false);
    let client_config = if hit.kind == "mcp" {
        // Same shape the CLI prints after an mcp push — paste-ready.
        let headers = if mcp_public {
            ""
        } else {
            ",\n      \"headers\": { \"Authorization\": \"Bearer <your rusted api key>\" }"
        };
        format!(
            "{{\n  \"mcpServers\": {{\n    \"{name}\": {{\n      \"url\": \"{url}\"{headers}\n    }}\n  }}\n}}"
        )
    } else {
        String::new()
    };
    let access = match hit.public {
        Some(true) => "public",
        Some(false) => "private",
        None if state.0.app.require_auth => "key required",
        None => "public",
    };
    let inner = FunctionT {
        name: name.clone(),
        published: hit.published,
        env_urls,
        deployed_at,
        deployed_ago,
        methods: trigger.methods.clone(),
        url_example: url.clone(),
        revision: hit.rev,
        size: pretty_size(source.len()),
        hidden_kb: hidden / 1024,
        code_json,
        kind: hit.kind.clone(),
        via,
        access,
        tools,
        mcp_public,
        client_config,
    }
    .render()
    .expect("lambda renders");
    console_page(&state, &headers, &user, &name, inner).await
}

/// Deletes the function — the same operation as `rusted delete`, gated on the
/// session owner actually owning it. State survives (purge is separate).
async fn function_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let app = &state.0.app;
    match app.store.owner(&name).await {
        Ok(Some(owner)) if owner == user.id => {}
        _ => return (StatusCode::NOT_FOUND, Html(String::new())).into_response(),
    }
    let _ = app.store.delete(&name).await;
    // htmx follows this to the dashboard; the deleted function's sidebar
    // entry disappears with the full page render.
    ([("hx-redirect", "/console")], Html(String::new())).into_response()
}

#[derive(Deserialize)]
struct PublishForm {
    published: String,
}

/// Flips the serving toggle and re-renders the page, so the banner and button
/// always reflect what the data plane is now doing.
async fn function_publish(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<PublishForm>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let publish = form.published == "true";
    let _ = state
        .0
        .app
        .store
        .set_published(&name, user.id, publish)
        .await;
    page_function(State(state), headers, Path(name)).await
}

async fn legacy_lambda_redirect(Path(name): Path<String>) -> Response {
    Redirect::permanent(&format!("/console/function/{name}")).into_response()
}

fn missing_lambda(name: &str) -> String {
    // The name is whatever the URL said, not a deployed function — escape it
    // because this string is rendered through `inner|safe`.
    let name = escape_html(name);
    format!(
        r#"<div class="mx-auto max-w-3xl px-8 py-16 text-center">
             <h1 class="font-display text-2xl font-light">ƒ {name} isn't deployed</h1>
             <p class="mt-2 font-mono text-sm text-clay">push it from the CLI, then refresh</p>
           </div>"#
    )
}

// ------------------------------------------------------------------- test proxy

#[derive(Deserialize)]
struct TestForm {
    method: String,
    url: String,
    /// Sent as `Authorization: Bearer …` when present — needed once the server
    /// runs with `--require-auth`.
    #[serde(default)]
    token: String,
    #[serde(default)]
    headers_json: String,
    #[serde(default)]
    body: String,
}

async fn run_test(
    State(state): State<WebState>,
    headers: HeaderMap,
    Form(form): Form<TestForm>,
) -> Response {
    if require_user(&state, &headers).await.is_err() {
        return Redirect::to("/login").into_response();
    }
    let data_origin = state.0.app.data_url("");
    if !form.url.starts_with(&data_origin) {
        return Html(
            TestResultT {
                ok: false,
                status: "blocked".into(),
                took_ms: "0".into(),
                content_type: String::new(),
                body: format!(
                    "test requests can only target this server's functions ({data_origin}…)"
                ),
            }
            .render()
            .expect("result renders"),
        )
        .into_response();
    }
    let method =
        reqwest::Method::from_bytes(form.method.as_bytes()).unwrap_or(reqwest::Method::POST);
    let has_body = matches!(method.as_str(), "POST" | "PUT" | "PATCH");
    let mut request = state.0.http.request(method, &form.url);
    if let Ok(extra) =
        serde_json::from_str::<std::collections::BTreeMap<String, String>>(&form.headers_json)
    {
        for (k, v) in extra {
            request = request.header(k, v);
        }
    }
    let token = form.token.trim();
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    if has_body && !form.body.is_empty() {
        request = request.body(form.body);
    }
    let started = Instant::now();
    let result = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            return Html(
                TestResultT {
                    ok: false,
                    status: "unreachable".into(),
                    took_ms: format!("{:.0}", started.elapsed().as_secs_f64() * 1000.0),
                    content_type: String::new(),
                    body: e.to_string(),
                }
                .render()
                .expect("result renders"),
            )
            .into_response()
        }
    };
    let took_ms = format!("{:.1}", started.elapsed().as_secs_f64() * 1000.0);
    let status_code = result.status();
    let content_type = result
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = result.text().await.unwrap_or_default();
    let body = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(text),
        Err(_) => text,
    };
    Html(
        TestResultT {
            ok: status_code.is_success(),
            status: format!(
                "{} {}",
                status_code.as_u16(),
                status_code.canonical_reason().unwrap_or("")
            ),
            took_ms,
            content_type,
            body,
        }
        .render()
        .expect("result renders"),
    )
    .into_response()
}

// ---------------------------------------------------------------- database

#[derive(Template)]
#[template(path = "database.html")]
struct DatabaseT {
    envs: Vec<String>,
    env: String,
    size_label: String,
    cap_label: String,
}

async fn page_database(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<EnvQuery>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    let envs = crate::secrets::list_envs(&state.0.app.pool, user.id).await;
    let env = query
        .env
        .filter(|e| envs.iter().any(|known| known == e))
        .unwrap_or_else(|| crate::secrets::PROD_ENV.to_string());
    let size = state.0.app.appdb.size_on_disk(user.id, &env);
    let inner = DatabaseT {
        envs,
        env,
        size_label: pretty_size(size as usize),
        cap_label: pretty_size(crate::appdb::DB_MAX_BYTES as usize),
    }
    .render()
    .expect("database renders");
    console_page(&state, &headers, &user, "database", inner).await
}

#[derive(Deserialize)]
struct DatabaseSqlBody {
    #[serde(default)]
    env: Option<String>,
    sql: String,
}

/// The console's SQL runner: session-authenticated, same authorizer and a
/// bounded deadline — the console can do nothing a function couldn't.
async fn database_sql(
    State(state): State<WebState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<DatabaseSqlBody>,
) -> Response {
    let user = match editor_user(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let env = req
        .env
        .unwrap_or_else(|| crate::secrets::PROD_ENV.to_string());
    if env != crate::secrets::PROD_ENV
        && !crate::secrets::env_exists(&state.0.app.pool, user.id, &env).await
    {
        return axum::Json(serde_json::json!({ "error": "no such environment" })).into_response();
    }
    // SELECT-shaped statements return rows; everything else reports changes —
    // so the console shows "✓ ok — 1 change" for DDL instead of an empty grid.
    let head = req.sql.trim_start().to_lowercase();
    let is_query = ["select", "with", "values", "explain"]
        .iter()
        .any(|k| head.starts_with(k));
    let op = serde_json::json!({
        "op": if is_query { "query" } else { "exec" },
        "sql": req.sql,
        "params": [],
    })
    .to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    match state.0.app.appdb.run(user.id, &env, op, deadline).await {
        Ok(result) => Html(result).into_response(),
        Err(message) => axum::Json(serde_json::json!({ "error": message })).into_response(),
    }
}

// ------------------------------------------------------------------ editor

/// Blank-buffer starting points, one per surface. The declarations are all
/// visible — commented where optional — so the module is its own reference:
/// the name lives in the file, not in a form field.
const EDITOR_SCAFFOLD_HTTP: &str = r#"export const http = {
  name: "my-function",        // becomes /f/my-function
  methods: ["POST"],          // e.g. ["GET", "POST"]
  // path: "/users/{id}",     // optional route; captures in request.params
  // access: "private",       // "public" | "private"; unset follows the server
};

export const config = {
  // secrets: ["GITHUB_TOKEN"],  // vault names, decrypted into context.env
  // state: true,                // durable context.state
};

export default async function handler(request: Rusted.Request, context: Rusted.Context) {
  // .catch: a bare POST has no body, and that should greet, not throw.
  const { name } = await request.json<{ name?: string }>().catch(() => ({}) as { name?: string });
  return context.json({ message: `Hello, ${name ?? "world"}` });
}
"#;

const EDITOR_SCAFFOLD_MCP: &str = r#"export const mcp: Rusted.Mcp = {
  name: "my-tools",           // becomes /f/my-tools
  // public: true,            // serve without a key; default needs your key
  tools: {
    hello: {
      description: "Say hello",
      inputSchema: {
        type: "object",
        properties: { name: { type: "string" } },
        required: ["name"],
      },
      async handler({ name }: { name: string }) {
        return `Hello, ${name}!`;
      },
    },
  },
};

export const config = {
  // secrets: ["GITHUB_TOKEN"],  // vault names, decrypted into context.env
};
"#;

const EDITOR_SCAFFOLD_APP: &str = r#"export const app = rusted
  .app({
    name: "my-app",           // becomes /f/my-app
    // access: "private",     // "public" | "private"; unset follows the server
  })
  .use(async (request, context, next) => {
    // runs before every matched route; return a response to short-circuit
    return next();
  })
  .get("/", home)
  .get("/hello/{who}", hello); // captures land in request.params

async function home(request: Rusted.Request, context: Rusted.Context) {
  return context.json({ routes: ["/", "/hello/{who}"] });
}

async function hello(request: Rusted.Request, context: Rusted.Context) {
  return context.json({ message: `Hello, ${request.params.who}` });
}

export const config = {
  // db: true,                   // shared SQL database on context.db
  // secrets: ["GITHUB_TOKEN"],  // vault names, decrypted into context.env
};
"#;

#[derive(Template)]
#[template(path = "editor.html")]
struct EditorT {
    /// The initial buffer as a JSON string literal, safe inside <script>.
    source_json: String,
    name: String,
    /// Raw origin of the loaded function's serving revision ("cli", "editor",
    /// "agent"), empty for a blank buffer — drives the fork warning.
    origin: String,
    origin_label: String,
    /// True when ?kind= asked for a fresh scaffold: the scratch draft is
    /// replaced instead of restored.
    fresh: bool,
}

#[derive(Deserialize, Default)]
struct EditorPageQuery {
    #[serde(default)]
    name: Option<String>,
    /// "http", "mcp" or "app": open a fresh scaffold of that surface,
    /// replacing any scratch draft — the explicit choice made in the
    /// new-function dialog.
    #[serde(default)]
    kind: Option<String>,
}

async fn page_editor(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<EditorPageQuery>,
) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    // ?name= loads a function you own; anything else opens a blank buffer —
    // the editor is not a way to read other people's source.
    let scaffold = || {
        match query.kind.as_deref() {
            Some("mcp") => EDITOR_SCAFFOLD_MCP,
            Some("app") => EDITOR_SCAFFOLD_APP,
            _ => EDITOR_SCAFFOLD_HTTP,
        }
        .to_string()
    };
    let (name, source) = match &query.name {
        Some(wanted) => match state.0.app.store.fetch(wanted).await {
            Ok(Some(hit)) if hit.owner == Some(user.id) => (wanted.clone(), hit.source.clone()),
            _ => (String::new(), scaffold()),
        },
        None => (String::new(), scaffold()),
    };
    let origin = if name.is_empty() {
        String::new()
    } else {
        crate::api::previous_via(&state.0.app, &name)
            .await
            .unwrap_or_default()
    };
    let inner = EditorT {
        source_json: serde_json::to_string(&source)
            .expect("strings serialize")
            .replace("</", "<\\/"),
        name,
        origin_label: via_label(&origin).to_string(),
        origin,
        fresh: query.kind.is_some(),
    }
    .render()
    .expect("editor renders");
    console_page(&state, &headers, &user, "editor", inner).await
}

/// One sidebar entry: the function and which surface pushed its serving
/// revision, for the little origin marker.
struct NavFn {
    name: String,
    via: String,
    via_label: String,
}

#[derive(Template)]
#[template(path = "nav_functions.html")]
struct NavFunctionsT {
    lambdas: Vec<NavFn>,
    active: String,
}

async fn nav_rows(state: &WebState, user_id: Uuid) -> Vec<NavFn> {
    sqlx::query(
        "SELECT f.name, coalesce(r.via, 'cli') AS via
         FROM functions f
         LEFT JOIN revisions r ON r.function_name = f.name AND r.rev = f.current_rev
         WHERE f.user_id = $1 ORDER BY f.name",
    )
    .bind(user_id)
    .fetch_all(&state.0.app.pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| {
        let via: String = row.get("via");
        NavFn {
            name: row.get("name"),
            via_label: via_label(&via).to_string(),
            via,
        }
    })
    .collect()
}

/// The sidebar's function list as a fragment, so the editor can refresh it
/// after a push without reloading the page (and losing the buffer).
async fn nav_functions(State(state): State<WebState>, headers: HeaderMap) -> Response {
    let user = match require_user(&state, &headers).await {
        Ok(user) => user,
        Err(redirect) => return redirect,
    };
    Html(
        NavFunctionsT {
            lambdas: nav_rows(&state, user.id).await,
            active: String::new(),
        }
        .render()
        .expect("nav renders"),
    )
    .into_response()
}

/// The editor endpoints answer fetch() calls: a missing session is a JSON 401
/// the page surfaces, not a redirect the fetch would silently follow.
async fn editor_user(state: &WebState, headers: &HeaderMap) -> Result<User, Response> {
    match current_user(state, headers).await {
        Some(user) => Ok(user),
        None => Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(
                serde_json::json!({ "error": { "code": "unauthorized", "message": "signed out" } }),
            ),
        )
            .into_response()),
    }
}

#[derive(Deserialize)]
struct EditorRunBody {
    source: String,
    #[serde(default)]
    body: String,
}

async fn editor_run(
    State(state): State<WebState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<EditorRunBody>,
) -> Response {
    let user = match editor_user(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    crate::api::run_adhoc(&state.0.app, user.id, req.source, req.body).await
}

#[derive(Deserialize)]
struct EditorSourceBody {
    source: String,
}

async fn editor_verify(
    State(state): State<WebState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<EditorSourceBody>,
) -> Response {
    if let Err(response) = editor_user(&state, &headers).await {
        return response;
    }
    match crate::api::inspect_source(&state.0.app, req.source).await {
        Ok(inspection) => {
            let value = match &inspection.surface {
                rusted_engine::Surface::Http(_) => {
                    serde_json::json!({ "ok": true, "kind": "http" })
                }
                rusted_engine::Surface::Mcp(c) => serde_json::json!({
                    "ok": true,
                    "kind": "mcp",
                    "tools": c.tools.keys().collect::<Vec<_>>(),
                }),
                rusted_engine::Surface::App(c) => serde_json::json!({
                    "ok": true,
                    "kind": "app",
                    "routes": c
                        .routes
                        .iter()
                        .map(|r| format!("{} {}", r.method, r.path))
                        .collect::<Vec<_>>(),
                }),
            };
            axum::Json(value).into_response()
        }
        Err(message) => {
            axum::Json(serde_json::json!({ "ok": false, "message": message })).into_response()
        }
    }
}

#[derive(Deserialize)]
struct EditorPushBody {
    source: String,
    #[serde(default)]
    name: Option<String>,
}

async fn editor_push(
    State(state): State<WebState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<EditorPushBody>,
) -> Response {
    let user = match editor_user(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    match crate::api::deploy_function(
        &state.0.app,
        user.id,
        req.source,
        req.name,
        None,
        None,
        "editor",
    )
    .await
    {
        Ok(value) => axum::Json(value).into_response(),
        Err(refused) => (
            refused.status,
            axum::Json(serde_json::json!({ "error": {
                "code": refused.code,
                "message": refused.message,
            }})),
        )
            .into_response(),
    }
}

// ------------------------------------------------------------------- admin

/// Signed-in non-admins get a plain 404: to everyone without the flag,
/// /console/admin does not exist. Anonymous visitors bounce to /login like any
/// console page.
async fn require_admin(state: &WebState, headers: &HeaderMap) -> Result<User, Response> {
    let user = require_user(state, headers).await?;
    if user.admin {
        Ok(user)
    } else {
        Err((StatusCode::NOT_FOUND, "not found").into_response())
    }
}

/// One page of a paged admin table, and whether more follows.
const ADMIN_PAGE_SIZE: i64 = 25;

/// `ILIKE` treats `%`, `_`, and `\` specially; a search for a literal
/// percent sign should find one.
fn like_escape(q: &str) -> String {
    q.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

struct AdminRecentUser {
    login: String,
    email: String,
    when: String,
}

struct AdminRecentFn {
    name: String,
    owner: String,
    when: String,
}

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminT {
    total_users: String,
    total_functions: String,
    month_invocations: String,
    recent_users: Vec<AdminRecentUser>,
    recent_functions: Vec<AdminRecentFn>,
}

async fn page_admin(State(state): State<WebState>, headers: HeaderMap) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let pool = &state.0.app.pool;
    // Retention pruning trims invocation rows per plan, so the monthly count
    // is "within retention" — the template says so.
    let totals = sqlx::query(
        "SELECT (SELECT count(*) FROM users) AS users,
                (SELECT count(*) FROM functions) AS functions,
                (SELECT count(*) FROM invocations
                  WHERE at >= date_trunc('month', now())) AS month_invocations",
    )
    .fetch_one(pool)
    .await;
    let (users, functions, month): (i64, i64, i64) = match &totals {
        Ok(row) => (
            row.get("users"),
            row.get("functions"),
            row.get("month_invocations"),
        ),
        Err(_) => (0, 0, 0),
    };
    let recent_users = sqlx::query(
        "SELECT login, email, extract(epoch FROM created_at)::bigint AS added
         FROM users ORDER BY created_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| AdminRecentUser {
        login: row.get("login"),
        email: row
            .get::<Option<String>, _>("email")
            .unwrap_or_else(|| "—".into()),
        when: ago(row.get("added")),
    })
    .collect();
    let recent_functions = sqlx::query(
        "SELECT f.name, coalesce(u.login, '—') AS owner,
                extract(epoch FROM f.created_at)::bigint AS added
         FROM functions f LEFT JOIN users u ON u.id = f.user_id
         ORDER BY f.created_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|row| AdminRecentFn {
        name: row.get("name"),
        owner: row.get("owner"),
        when: ago(row.get("added")),
    })
    .collect();
    let inner = AdminT {
        total_users: human_count(users),
        total_functions: human_count(functions),
        month_invocations: human_count(month),
        recent_users,
        recent_functions,
    }
    .render()
    .expect("admin renders");
    console_page(&state, &headers, &user, "admin", inner).await
}

// ------------------------------------------------------------- admin: users

#[derive(Deserialize, Default)]
struct AdminListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    page: Option<i64>,
}

impl AdminListQuery {
    fn page(&self) -> i64 {
        self.page.unwrap_or(1).max(1)
    }
    /// (sort, dir) validated against this page's whitelist — anything else
    /// falls back to the default, so the ORDER BY below is never user text.
    fn sort_dir<'k>(&self, keys: &[&'k str], default: (&'k str, &'k str)) -> (&'k str, &'k str) {
        let sort = keys
            .iter()
            .find(|k| Some(**k) == self.sort.as_deref())
            .copied()
            .unwrap_or(default.0);
        let dir = match self.dir.as_deref() {
            Some("asc") => "asc",
            Some("desc") => "desc",
            _ => default.1,
        };
        (sort, dir)
    }
    fn url(&self, base: &str, sort: &str, dir: &str, page: i64) -> String {
        format!(
            "{base}?q={}&sort={sort}&dir={dir}&page={page}",
            urlencoding::encode(&self.q)
        )
    }
}

struct AdminUserRow {
    id: String,
    login: String,
    email: String,
    plan: String,
    added: String,
    last_login: String,
    admin: bool,
    /// The signed-in admin cannot toggle themselves; the button hides.
    is_self: bool,
}

#[derive(Template)]
#[template(path = "admin_users.html")]
struct AdminUsersT {
    rows: Vec<AdminUserRow>,
    q: String,
    sort: String,
    dir: String,
    page: i64,
    /// Header links flip direction when re-sorting the active column.
    added_url: String,
    login_url: String,
    prev_url: Option<String>,
    next_url: Option<String>,
    toggle_query: String,
    error: Option<String>,
}

async fn admin_users_inner(
    state: &WebState,
    user: &User,
    query: &AdminListQuery,
    error: Option<String>,
) -> String {
    let (sort, dir) = query.sort_dir(&["added", "lastlogin"], ("added", "desc"));
    let order = match (sort, dir) {
        ("added", "asc") => "u.created_at ASC",
        ("added", _) => "u.created_at DESC",
        ("lastlogin", "asc") => "u.last_login_at ASC NULLS FIRST",
        _ => "u.last_login_at DESC NULLS LAST",
    };
    let page = query.page();
    let pattern = format!("%{}%", like_escape(query.q.trim()));
    // AssertSqlSafe: {order} interpolates one of four literals from the
    // whitelist above, never caller text.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT u.id, u.login, u.email, u.admin,
                extract(epoch FROM u.created_at)::bigint AS added,
                extract(epoch FROM u.last_login_at)::bigint AS last_login,
                p.name AS plan
         FROM users u
         LEFT JOIN LATERAL (
             SELECT pl.name FROM subscriptions s JOIN plans pl ON pl.id = s.plan_id
             WHERE s.user_id = u.id AND s.status = 'active'
             ORDER BY s.started_at DESC LIMIT 1
         ) p ON TRUE
         WHERE $1 = '' OR u.email ILIKE $2 OR u.login ILIKE $2
         ORDER BY {order} LIMIT $3 OFFSET $4"
    )))
    .bind(query.q.trim())
    .bind(&pattern)
    .bind(ADMIN_PAGE_SIZE + 1)
    .bind((page - 1) * ADMIN_PAGE_SIZE)
    .fetch_all(&state.0.app.pool)
    .await
    .unwrap_or_default();
    let has_next = rows.len() as i64 > ADMIN_PAGE_SIZE;
    let rows = rows
        .into_iter()
        .take(ADMIN_PAGE_SIZE as usize)
        .map(|row| {
            let id: Uuid = row.get("id");
            AdminUserRow {
                id: id.to_string(),
                login: row.get("login"),
                email: row
                    .get::<Option<String>, _>("email")
                    .unwrap_or_else(|| "—".into()),
                plan: row
                    .get::<Option<String>, _>("plan")
                    .unwrap_or_else(|| "Dev".into()),
                added: ago(row.get("added")),
                last_login: row
                    .get::<Option<i64>, _>("last_login")
                    .map(ago)
                    .unwrap_or_else(|| "never".into()),
                admin: row.get("admin"),
                is_self: id == user.id,
            }
        })
        .collect();
    let base = "/console/admin/users";
    let flip = |key: &str| {
        if sort == key && dir == "desc" {
            "asc"
        } else {
            "desc"
        }
    };
    AdminUsersT {
        added_url: query.url(base, "added", flip("added"), 1),
        login_url: query.url(base, "lastlogin", flip("lastlogin"), 1),
        prev_url: (page > 1).then(|| query.url(base, sort, dir, page - 1)),
        next_url: has_next.then(|| query.url(base, sort, dir, page + 1)),
        toggle_query: format!(
            "q={}&sort={sort}&dir={dir}&page={page}",
            urlencoding::encode(query.q.trim())
        ),
        rows,
        q: query.q.trim().to_string(),
        sort: sort.to_string(),
        dir: dir.to_string(),
        page,
        error,
    }
    .render()
    .expect("admin users renders")
}

async fn page_admin_users(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<AdminListQuery>,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let inner = admin_users_inner(&state, &user, &query, None).await;
    console_page(&state, &headers, &user, "admin-users", inner).await
}

async fn admin_toggle_admin(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<AdminListQuery>,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    // Toggling yourself off would lock the last admin out mid-click; the
    // database is the way back in, so refuse the footgun outright.
    let error = if id == user.id {
        Some("you cannot change your own admin flag".to_string())
    } else {
        let updated = sqlx::query("UPDATE users SET admin = NOT admin WHERE id = $1")
            .bind(id)
            .execute(&state.0.app.pool)
            .await;
        match updated {
            Ok(result) if result.rows_affected() == 1 => {
                // Cached sessions still carry the old flag; drop them all so
                // the change is effective now, not in five minutes.
                state.0.app.auth.clear();
                None
            }
            Ok(_) => Some("no such user".to_string()),
            Err(e) => Some(e.to_string()),
        }
    };
    Html(admin_users_inner(&state, &user, &query, error).await).into_response()
}

// --------------------------------------------------------- admin: functions

struct AdminFnRow {
    name: String,
    owner: String,
    rev: i64,
    via: String,
    access: &'static str,
    created: String,
    updated: String,
}

#[derive(Template)]
#[template(path = "admin_functions.html")]
struct AdminFunctionsT {
    rows: Vec<AdminFnRow>,
    q: String,
    page: i64,
    created_url: String,
    updated_url: String,
    prev_url: Option<String>,
    next_url: Option<String>,
}

async fn page_admin_functions(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<AdminListQuery>,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (sort, dir) = query.sort_dir(&["created", "updated"], ("updated", "desc"));
    let order = match (sort, dir) {
        ("created", "asc") => "f.created_at ASC",
        ("created", _) => "f.created_at DESC",
        ("updated", "asc") => "f.updated_at ASC",
        _ => "f.updated_at DESC",
    };
    let page = query.page();
    let pattern = format!("%{}%", like_escape(query.q.trim()));
    // AssertSqlSafe: {order} interpolates one of four literals from the
    // whitelist above, never caller text.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT f.name, f.current_rev, f.public, coalesce(u.login, '—') AS owner,
                coalesce(r.via, 'cli') AS via,
                extract(epoch FROM f.created_at)::bigint AS created,
                extract(epoch FROM f.updated_at)::bigint AS updated
         FROM functions f LEFT JOIN users u ON u.id = f.user_id
         LEFT JOIN revisions r ON r.function_name = f.name AND r.rev = f.current_rev
         WHERE $1 = '' OR f.name ILIKE $2
         ORDER BY {order} LIMIT $3 OFFSET $4"
    )))
    .bind(query.q.trim())
    .bind(&pattern)
    .bind(ADMIN_PAGE_SIZE + 1)
    .bind((page - 1) * ADMIN_PAGE_SIZE)
    .fetch_all(&state.0.app.pool)
    .await
    .unwrap_or_default();
    let has_next = rows.len() as i64 > ADMIN_PAGE_SIZE;
    let rows = rows
        .into_iter()
        .take(ADMIN_PAGE_SIZE as usize)
        .map(|row| AdminFnRow {
            name: row.get("name"),
            owner: row.get("owner"),
            rev: row.get("current_rev"),
            via: via_label(row.get::<String, _>("via").as_str()).to_string(),
            access: match row.get::<Option<bool>, _>("public") {
                Some(true) => "public",
                Some(false) => "private",
                None if state.0.app.require_auth => "key required",
                None => "public",
            },
            created: ago(row.get("created")),
            updated: ago(row.get("updated")),
        })
        .collect();
    let base = "/console/admin/functions";
    let flip = |key: &str| {
        if sort == key && dir == "desc" {
            "asc"
        } else {
            "desc"
        }
    };
    let inner = AdminFunctionsT {
        created_url: query.url(base, "created", flip("created"), 1),
        updated_url: query.url(base, "updated", flip("updated"), 1),
        prev_url: (page > 1).then(|| query.url(base, sort, dir, page - 1)),
        next_url: has_next.then(|| query.url(base, sort, dir, page + 1)),
        rows,
        q: query.q.trim().to_string(),
        page,
    }
    .render()
    .expect("admin functions renders");
    console_page(&state, &headers, &user, "admin-functions", inner).await
}

#[cfg(test)]
mod split_user_code_tests {
    use super::split_user_code;

    #[test]
    fn cli_rolldown_bundles_fold_node_modules() {
        let source = "//#region node_modules/ms/index.js
var ms = 1;
//#endregion
//#region index.js
export default ms;
";
        let (user, hidden) = split_user_code(source);
        assert!(user.starts_with("//#region index.js"));
        assert!(hidden > 0);
    }

    #[test]
    fn editor_esbuild_bundles_fold_http_modules() {
        // The console editor bundles npm through esm.sh: dependency modules
        // are labeled with their URL namespace, the user's files with vfs:.
        let source = "// http:https://esm.sh/slugify@1.6.9/es2020/slugify.mjs
var S = () => {};

// vfs:index.js
var index_default = async () => S();
export { index_default as default };
";
        let (user, hidden) = split_user_code(source);
        assert!(
            user.starts_with("// vfs:index.js"),
            "expected the vfs entry, got: {user}"
        );
        assert_eq!(hidden, source.find("// vfs:index.js").unwrap());
    }

    #[test]
    fn unbundled_source_shows_whole() {
        let source = "export default async function handler() { return 1; }
";
        let (user, hidden) = split_user_code(source);
        assert_eq!(user, source);
        assert_eq!(hidden, 0);
    }
}

#[cfg(test)]
mod login_response_tests {
    use super::*;

    /// Both cookies must survive into the response — the session cookie
    /// being silently replaced by the cleanup cookie is the regression this
    /// guards against.
    #[test]
    fn login_success_carries_both_cookies() {
        let response = login_success_response("token-value", "/console");
        let cookies: Vec<&str> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(cookies.len(), 2, "{cookies:?}");
        assert!(
            cookies[0].starts_with("rusted_session=token-value;"),
            "{cookies:?}"
        );
        assert!(cookies[0].contains("HttpOnly") && cookies[0].contains("Secure"));
        assert!(
            cookies[1].starts_with("rusted_after_login=;"),
            "{cookies:?}"
        );
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
}

#[cfg(test)]
mod google_claims_tests {
    use super::parse_google_claims;
    use base64::Engine;

    fn token(payload: serde_json::Value) -> String {
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
        format!("e30.{body}.unverified-signature")
    }

    #[test]
    fn accepts_google_and_rejects_wrong_audience_issuer_or_expiry() {
        let good = serde_json::json!({
            "iss": "https://accounts.google.com", "aud": "client-1",
            "sub": "g-123", "exp": 2_000,
            "email": "a@x.io", "email_verified": true,
        });
        let claims = parse_google_claims(&token(good.clone()), "client-1", 1_000).unwrap();
        assert_eq!(claims.sub, "g-123");
        assert!(claims.email_verified);

        let mut wrong_aud = good.clone();
        wrong_aud["aud"] = "someone-else".into();
        assert!(parse_google_claims(&token(wrong_aud), "client-1", 1_000).is_none());

        let mut wrong_iss = good.clone();
        wrong_iss["iss"] = "https://evil.example".into();
        assert!(parse_google_claims(&token(wrong_iss), "client-1", 1_000).is_none());

        assert!(
            parse_google_claims(&token(good), "client-1", 3_000).is_none(),
            "expired token accepted"
        );
        assert!(parse_google_claims("not-a-jwt", "client-1", 1_000).is_none());
    }
}

#[cfg(test)]
mod github_email_tests {
    use super::{pick_github_email, GithubEmail};

    fn e(email: &str, primary: bool, verified: bool) -> GithubEmail {
        GithubEmail {
            email: email.into(),
            primary,
            verified,
        }
    }

    #[test]
    fn primary_verified_wins_and_unverified_never_surfaces() {
        assert_eq!(
            pick_github_email(vec![e("a@x.io", false, true), e("b@x.io", true, true)]),
            Some("b@x.io".into())
        );
        // An unverified primary is a claim, not an address: fall back to the
        // verified one.
        assert_eq!(
            pick_github_email(vec![e("a@x.io", false, true), e("b@x.io", true, false)]),
            Some("a@x.io".into())
        );
        assert_eq!(pick_github_email(vec![e("b@x.io", true, false)]), None);
        assert_eq!(pick_github_email(vec![]), None);
    }
}

#[cfg(test)]
mod llms_tests {
    use super::html_to_text;

    #[test]
    fn html_to_text_keeps_structure_and_drops_markup() {
        let html = "<h1>Title</h1>\n<p class=\"lede\">A &lt;fresh&gt; sandbox &amp; more.</p>\n<h2>Steps</h2>\n<ul><li>first</li><li>second</li></ul>\n<pre><code>rusted push <span class=\"cmt\"># deploy</span></code></pre>";
        let text = html_to_text(html);
        assert!(text.starts_with("# Title"), "{text}");
        assert!(text.contains("A <fresh> sandbox & more."));
        assert!(text.contains("## Steps"));
        assert!(text.contains("- first"));
        assert!(text.contains("```\nrusted push # deploy\n```"), "{text}");
        // Decoded entities may contain '<' (that's the point); real tags
        // must not survive.
        for leak in ["<p", "</", "<span", "<pre", "class="] {
            assert!(!text.contains(leak), "markup leaked ({leak}): {text}");
        }
    }
}

#[cfg(test)]
mod static_asset_tests {
    /// The Tailwind sheet is compiled offline (`make css`) and inlined into
    /// every page; nothing at build time proves it matches the templates. A
    /// canary set of classes — one per page family, including arbitrary-value
    /// ones whose escaped selectors are easy to lose — catches a stale or
    /// truncated sheet before it ships unstyled pages.
    #[test]
    fn compiled_stylesheet_covers_the_templates() {
        let sheet = include_str!("../templates/app.css");
        assert!(sheet.len() > 20_000, "app.css suspiciously small");
        for canary in [
            ".bg-rust-950",
            ".font-display",
            ".bg-ember\\/10",
            ".lg\\:grid-cols-\\[1\\.02fr_\\.98fr\\]",
            ".tracking-\\[-0\\.04em\\]",
            ".btn-primary",
            ".console-nav",
            ".htmx-indicator",
            "@font-face",
            "Bricolage Grotesque",
            "JetBrains Mono",
        ] {
            assert!(
                sheet.contains(canary),
                "app.css lost {canary} — run `make css`"
            );
        }
    }

    /// Every page must render from this binary alone: a third-party origin in
    /// a template reintroduces the render-blocking requests this design
    /// removed (and a CSP/availability dependency with them).
    #[test]
    fn no_template_references_a_third_party_origin() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/templates");
        let mut queue = vec![std::path::PathBuf::from(dir)];
        while let Some(path) = queue.pop() {
            for entry in std::fs::read_dir(&path).expect("templates readable") {
                let entry = entry.expect("entry readable");
                if entry.file_type().expect("file type").is_dir() {
                    queue.push(entry.path());
                    continue;
                }
                let name = entry.file_name();
                let templated = ["html", "css"]
                    .iter()
                    .any(|ext| entry.path().extension().is_some_and(|e| e == *ext));
                if !templated {
                    continue; // .DS_Store and friends
                }
                let text = std::fs::read_to_string(entry.path()).expect("template readable");
                // Monaco on the (auth-gated, lazy-loaded) function editor is
                // the one tolerated exception — vendoring its workers would
                // add megabytes to the binary for a page crawlers never see.
                for host in [
                    "cdn.tailwindcss.com",
                    "unpkg.com",
                    "fonts.googleapis.com",
                    "fonts.gstatic.com",
                ] {
                    assert!(
                        !text.contains(host),
                        "{name:?} references {host}; serve it from /assets instead"
                    );
                }
            }
        }
    }
}
