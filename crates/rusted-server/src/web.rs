//! The rusted console: htmx + tailwind pages served from the admin listener.
//! Backed by Postgres: real GitHub OAuth sessions, real API keys, live lambda
//! data. The dashboard keeps labeled sample data until analytics land.

use std::sync::Arc;
use std::time::Instant;

use askama::Template;
use axum::extract::{Form, Path, Query, RawQuery, State};
use axum::http::header::{HeaderMap, SET_COOKIE};
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
}

#[derive(Clone)]
pub struct WebState(Arc<WebInner>);

pub struct OauthConfig {
    client_id: String,
    client_secret: String,
    callback_url: String,
}

impl OauthConfig {
    fn from_env() -> Option<OauthConfig> {
        Some(OauthConfig {
            client_id: std::env::var("GITHUB_CLIENT_ID").ok()?,
            client_secret: std::env::var("GITHUB_CLIENT_SECRET").ok()?,
            callback_url: std::env::var("GITHUB_CALLBACK_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:7412/auth/github/callback".to_string()),
        })
    }
}

impl WebState {
    pub fn new(app: Arc<AppState>) -> Self {
        WebState(Arc::new(WebInner {
            app,
            http: reqwest::Client::new(),
            oauth: OauthConfig::from_env(),
        }))
    }
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(landing))
        .route("/login", get(login))
        .route(
            "/oauth/authorize",
            get(oauth_authorize).post(oauth_authorize_decide),
        )
        .route("/auth/github", get(auth_github))
        .route("/auth/github/callback", get(auth_github_callback))
        .route("/logout", get(logout))
        .route("/device", get(device_page).post(device_decide))
        .route("/console", get(console_home))
        .route("/console/dashboard", get(page_dashboard))
        .route("/console/invocations", get(page_invocations))
        .route("/console/keys", get(page_keys).post(key_create))
        .route("/console/billing", get(page_billing))
        .route(
            "/console/checkout/{code}",
            get(page_checkout).post(confirm_checkout),
        )
        .route("/console/keys/{id}", delete(key_revoke))
        .route("/console/lambda/{name}", get(page_lambda))
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
        "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=read:user&state={csrf}",
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
    let pool = &state.0.app.pool;
    let user_id = match auth::upsert_github_user(
        pool,
        gh_user.id,
        &gh_user.login,
        gh_user.name.as_deref(),
        gh_user.avatar_url.as_deref(),
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
        .filter(|n| n.starts_with('/') && !n.starts_with("//"))
        .unwrap_or("/console")
        .to_string();
    (
        [
            (
                SET_COOKIE,
                format!(
                    "{}={session}; Path=/; HttpOnly; Max-Age=2592000; SameSite=Lax",
                    auth::SESSION_COOKIE
                ),
            ),
            (
                SET_COOKIE,
                "rusted_after_login=; Path=/; Max-Age=0".to_string(),
            ),
        ],
        Redirect::to(&next),
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

#[derive(Template)]
#[template(path = "login.html")]
struct LoginT {
    configured: bool,
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
    lambdas: Vec<String>,
    user_name: String,
    user_initial: String,
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

pub struct PlanCard {
    code: String,
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

#[derive(Template)]
#[template(path = "lambda.html")]
struct LambdaT {
    name: String,
    methods: Vec<String>,
    url: String,
    url_example: String,
    revision: u64,
    size: String,
    hidden_kb: usize,
    code_json: String,
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

async fn login(State(state): State<WebState>, Query(query): Query<LoginQuery>) -> Response {
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
    let lambdas = state
        .0
        .app
        .store
        .names_for_user(user.id)
        .await
        .unwrap_or_default();
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
            ok: row.outcome == "success",
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
    let summary = crate::analytics::summary(&app.pool, user.id, days).await;
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
    let delta = match (summary.invocations, summary.prior_invocations) {
        (_, 0) => "—".to_string(),
        (current, prior) => format!("{:+.0}%", (current - prior) as f64 / prior as f64 * 100.0),
    };
    let error_rate = if summary.invocations == 0 {
        "0%".to_string()
    } else {
        format!(
            "{:.1}%",
            summary.errors as f64 / summary.invocations as f64 * 100.0
        )
    };
    DashboardT {
        window: format!("last {days} days · {} plan retention", plan.name),
        stats: Stats {
            invocations: human_count(summary.invocations),
            invocations_delta: delta,
            p95_exec: format!("{:.1} ms", summary.p95_exec_ms),
            error_rate,
            errors: human_count(summary.errors),
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

fn card(plan: &crate::plans::Plan, current: bool) -> PlanCard {
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
    let inner = BillingT {
        current_name: current.name.clone(),
        plans: catalog
            .iter()
            .map(|plan| card(plan, plan.id == current.id))
            .collect(),
    }
    .render()
    .expect("billing renders");
    console_page(&state, &headers, &user, "billing", inner).await
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
    // The code comes from the URL, so an internal plan must not be reachable by
    // guessing its name.
    if !crate::plans::is_public(&code) {
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
            if path.starts_with("node_modules/") || path.starts_with('\0') {
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
    if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

async fn page_lambda(
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
    let (record, source) = (&hit.0, &hit.1);
    let trigger = record.trigger.clone();
    let route = format!("/f/{}{}", name, trigger.path.as_deref().unwrap_or(""));
    let url = state.0.app.data_url(&route);
    let (user_code, hidden) = split_user_code(source);
    let code_json = serde_json::json!({ "user": user_code, "full": source })
        .to_string()
        .replace("</", "<\\/");
    let inner = LambdaT {
        name: name.clone(),
        methods: trigger.methods.clone(),
        url_example: url.clone(),
        url,
        revision: record.current().rev,
        size: pretty_size(source.len()),
        hidden_kb: hidden / 1024,
        code_json,
    }
    .render()
    .expect("lambda renders");
    console_page(&state, &headers, &user, &name, inner).await
}

fn missing_lambda(name: &str) -> String {
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
