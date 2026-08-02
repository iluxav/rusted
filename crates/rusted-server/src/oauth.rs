//! OAuth 2.1 for MCP clients.
//!
//! A Bearer key already works for anything that can set a header. This exists
//! for the clients that cannot be handed one — hosted assistants that discover
//! a server, register themselves, and send the user through a browser. The MCP
//! authorization spec requires that path, and without it the browser and cloud
//! agents this is built for cannot connect at all.
//!
//! Access tokens are issued as ordinary API keys. That is deliberate: `/mcp`
//! authenticates the same way it always did, plans resolve the same way, the
//! cache and its invalidation are unchanged, and a user revokes an assistant
//! from the key list they already have.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::state::AppState;

/// How long an authorization code is good for. Long enough for a browser
/// redirect and a token call, short enough that a leaked one is stale.
const CODE_TTL_SECONDS: i64 = 300;

/// The origin this server is reached on, which every OAuth URL must agree with.
/// Falls back to the bound address so local development works unconfigured.
fn issuer(state: &AppState) -> String {
    state.console_url("")
}

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": code, "error_description": description })),
    )
        .into_response()
}

// ---------------------------------------------------------------- discovery

/// RFC 9728. The MCP spec requires this, and it is how a client learns which
/// authorization server to talk to before it has any credentials.
async fn protected_resource_metadata(State(state): State<Arc<AppState>>) -> Response {
    let issuer = issuer(&state);
    Json(json!({
        "resource": format!("{issuer}/mcp"),
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "resource_documentation": format!("{issuer}/docs"),
    }))
    .into_response()
}

/// RFC 8414. Advertises only what is actually implemented — claiming support
/// for a grant or method that is not there would fail later and less clearly.
async fn authorization_server_metadata(State(state): State<Arc<AppState>>) -> Response {
    let issuer = issuer(&state);
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "registration_endpoint": format!("{issuer}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        // No `plain`: PKCE is the only thing standing in for a client secret
        // here, and `plain` would give away the protection it exists for.
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["mcp"],
    }))
    .into_response()
}

// ------------------------------------------------------------- registration

#[derive(Deserialize)]
struct RegistrationRequest {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

/// A redirect target must be somewhere a browser can be sent safely. The spec
/// allows only HTTPS or loopback; anything else is a phishing hop waiting to
/// happen, and clients legitimately use loopback for desktop flows.
fn acceptable_redirect(uri: &str) -> bool {
    if uri.contains('#') {
        return false;
    }
    if uri.starts_with("https://") {
        return true;
    }
    uri.starts_with("http://127.0.0.1")
        || uri.starts_with("http://localhost")
        || uri.starts_with("http://[::1]")
}

/// RFC 7591. Clients cannot pre-register with a server they just discovered,
/// so they introduce themselves and receive an id.
async fn register(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegistrationRequest>,
) -> Response {
    if body.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "at least one redirect_uri is required",
        );
    }
    if let Some(bad) = body.redirect_uris.iter().find(|u| !acceptable_redirect(u)) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!("{bad} is not usable: redirect URIs must be https, or http on loopback, and cannot contain a fragment"),
        );
    }

    let client_id = format!("mcp_{}", crate::auth::random_token(16));
    let client_name = body
        .client_name
        .unwrap_or_else(|| "an MCP client".to_string());
    let inserted = sqlx::query(
        "INSERT INTO oauth_clients (client_id, client_name, redirect_uris) VALUES ($1, $2, $3)",
    )
    .bind(&client_id)
    .bind(&client_name)
    .bind(&body.redirect_uris)
    .execute(&state.pool)
    .await;
    if let Err(e) = inserted {
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            &format!("could not register: {e}"),
        );
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": client_id,
            "client_name": client_name,
            "redirect_uris": body.redirect_uris,
            // A public client: no secret to leak, PKCE proves possession.
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------- authorize

#[derive(Deserialize)]
pub struct AuthorizeParams {
    pub client_id: String,
    pub redirect_uri: String,
    #[serde(default)]
    pub response_type: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// A client that asked to connect, resolved for the consent screen.
pub struct PendingAuthorization {
    pub client_name: String,
    pub redirect_uri: String,
}

/// Validates an authorization request without granting anything.
///
/// Everything checkable is checked before a user is asked to approve, so a
/// malformed request fails as an error rather than as a confusing consent
/// screen — and an unregistered redirect URI never reaches a browser.
pub async fn validate_authorize(
    state: &AppState,
    params: &AuthorizeParams,
) -> Result<PendingAuthorization, String> {
    if params.response_type != "code" {
        return Err(format!(
            "unsupported response_type '{}': only 'code' is supported",
            params.response_type
        ));
    }
    if params.code_challenge_method != "S256" {
        return Err(
            "code_challenge_method must be S256; this server does not accept 'plain'".to_string(),
        );
    }
    if params.code_challenge.is_empty() {
        return Err("code_challenge is required".to_string());
    }

    let row =
        sqlx::query("SELECT client_name, redirect_uris FROM oauth_clients WHERE client_id = $1")
            .bind(&params.client_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| format!("could not read the client: {e}"))?
            .ok_or_else(|| format!("unknown client '{}'", params.client_id))?;

    let registered: Vec<String> = row.get("redirect_uris");
    // Exact match, as the spec requires: prefix matching is how open redirects
    // get in.
    if !registered.iter().any(|u| u == &params.redirect_uri) {
        return Err(format!(
            "redirect_uri '{}' was not registered by this client",
            params.redirect_uri
        ));
    }

    Ok(PendingAuthorization {
        client_name: row.get("client_name"),
        redirect_uri: params.redirect_uri.clone(),
    })
}

/// Issues a code once a user has approved, and returns where to send them.
pub async fn grant(
    state: &AppState,
    user_id: Uuid,
    params: &AuthorizeParams,
) -> Result<String, String> {
    // Re-validated rather than trusted: the approval came back from a browser,
    // and the parameters came with it.
    validate_authorize(state, params).await?;

    let code = crate::auth::random_token(32);
    sqlx::query(
        "INSERT INTO oauth_codes
           (code_hash, client_id, user_id, redirect_uri, code_challenge, resource, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
    )
    .bind(crate::auth::sha256_hex(&code))
    .bind(&params.client_id)
    .bind(user_id)
    .bind(&params.redirect_uri)
    .bind(&params.code_challenge)
    .bind(params.resource.as_deref())
    .bind(CODE_TTL_SECONDS as f64)
    .execute(&state.pool)
    .await
    .map_err(|e| format!("could not issue a code: {e}"))?;

    let mut location = format!(
        "{}{}code={}",
        params.redirect_uri,
        if params.redirect_uri.contains('?') {
            "&"
        } else {
            "?"
        },
        urlencoding::encode(&code)
    );
    if let Some(client_state) = &params.state {
        location.push_str(&format!("&state={}", urlencoding::encode(client_state)));
    }
    Ok(location)
}

// -------------------------------------------------------------------- token

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
}

/// PKCE S256: the verifier hashed and base64url-encoded without padding must
/// equal the challenge presented at authorize time.
fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    // Constant-time is overkill for a value the client just sent us, but the
    // comparison is cheap and the habit is right.
    computed.len() == challenge.len()
        && computed
            .bytes()
            .zip(challenge.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

async fn token(State(state): State<Arc<AppState>>, Form(body): Form<TokenRequest>) -> Response {
    if body.grant_type != "authorization_code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only authorization_code is supported",
        );
    }

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                &format!("could not start a transaction: {e}"),
            )
        }
    };

    // Locked and marked redeemed in one transaction, so a code cannot be spent
    // twice by two requests racing.
    let row = sqlx::query(
        "SELECT client_id, user_id, redirect_uri, code_challenge, resource,
                expires_at < now() AS expired, redeemed_at IS NOT NULL AS redeemed
           FROM oauth_codes WHERE code_hash = $1 FOR UPDATE",
    )
    .bind(crate::auth::sha256_hex(&body.code))
    .fetch_optional(&mut *tx)
    .await;

    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "that authorization code is not valid",
            )
        }
        Err(e) => {
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                &format!("could not read the code: {e}"),
            )
        }
    };

    let redeemed: bool = row.get("redeemed");
    if redeemed {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "that authorization code has already been used",
        );
    }
    let expired: bool = row.get("expired");
    if expired {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "that authorization code has expired",
        );
    }
    let client_id: String = row.get("client_id");
    if client_id != body.client_id {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "that code was issued to a different client",
        );
    }
    let redirect_uri: String = row.get("redirect_uri");
    if redirect_uri != body.redirect_uri {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "redirect_uri does not match the one the code was issued for",
        );
    }
    let challenge: String = row.get("code_challenge");
    if !pkce_matches(&body.code_verifier, &challenge) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "code_verifier does not match the challenge",
        );
    }

    let user_id: Uuid = row.get("user_id");
    if let Err(e) = sqlx::query("UPDATE oauth_codes SET redeemed_at = now() WHERE code_hash = $1")
        .bind(crate::auth::sha256_hex(&body.code))
        .execute(&mut *tx)
        .await
    {
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            &format!("could not redeem the code: {e}"),
        );
    }
    if let Err(e) = tx.commit().await {
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            &format!("could not commit: {e}"),
        );
    }

    // Issued as an ordinary API key, so /mcp authenticates unchanged and the
    // user can revoke this assistant from the key list they already have.
    let name = sqlx::query("SELECT client_name FROM oauth_clients WHERE client_id = $1")
        .bind(&client_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<String, _>("client_name"))
        .unwrap_or_else(|| "an MCP client".to_string());

    match crate::auth::create_key(&state.pool, user_id, &format!("mcp: {name}")).await {
        Ok((_, token)) => Json(json!({
            "access_token": token,
            "token_type": "Bearer",
            "scope": "mcp",
        }))
        .into_response(),
        Err(e) => oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            &format!("could not issue a token: {e}"),
        ),
    }
}

/// The 401 an unauthenticated MCP request gets, pointing at the metadata a
/// client needs to start the flow. Without this header a client has no way to
/// discover where to authorize, and the spec requires it.
pub fn unauthorized_challenge(state: &AppState) -> Response {
    let issuer = issuer(state);
    let mut response = oauth_error(
        StatusCode::UNAUTHORIZED,
        "invalid_token",
        "this endpoint needs an access token; see the linked metadata to obtain one",
    );
    response.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{issuer}/.well-known/oauth-protected-resource\""
        ))
        .expect("header is ascii"),
    );
    response
}

/// Everything that needs no browser session. `/oauth/authorize` lives with the
/// console, since it needs the signed-in user.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        // Clients look for resource metadata under the resource path too.
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/token", post(token))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_accepts_a_matching_verifier() {
        // Worked example from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(pkce_matches(verifier, challenge));
    }

    #[test]
    fn pkce_rejects_a_wrong_verifier() {
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(!pkce_matches("not-the-verifier", challenge));
        assert!(!pkce_matches("", challenge));
    }

    #[test]
    fn redirect_uris_must_be_https_or_loopback() {
        assert!(acceptable_redirect("https://claude.ai/api/mcp/callback"));
        assert!(acceptable_redirect("http://127.0.0.1:6274/callback"));
        assert!(acceptable_redirect("http://localhost:3000/cb"));

        assert!(!acceptable_redirect("http://evil.example.com/cb"));
        assert!(!acceptable_redirect("https://ok.example.com/cb#frag"));
        assert!(!acceptable_redirect("javascript:alert(1)"));
    }
}
