//! Authentication for deployed MCP resources.
//!
//! OAuth tokens are introspected by their declared issuer (RFC 7662), with
//! the introspection call authenticated by vault-held client credentials the
//! function itself can never read. Only the small, verified identity a tool
//! needs reaches JavaScript; bearer material is neither logged, cached in the
//! clear, nor forwarded to the function.
//!
//! Failure classes are kept apart on purpose: a token the issuer judged
//! invalid gets a 401 challenge, while an issuer that cannot be reached or
//! answers garbage is a 503 — telling a client to re-authorize because the
//! authorization server is down would send every user through consent for
//! nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use reqwest::redirect::Policy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::api::record_refusal;
use crate::state::AppState;
use crate::store::Fetched;

const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;
/// Authorization-server metadata rarely changes; five minutes bounds both
/// staleness and the discovery traffic a busy function generates.
const METADATA_TTL: Duration = Duration::from_secs(300);
const METADATA_CACHE_CAP: usize = 64;
/// Definitively-rejected tokens are remembered briefly so a client (or an
/// attacker) replaying the same bad token does not hammer the issuer. Only
/// issuer verdicts are cached — never issuer failures — and only by hash.
const NEGATIVE_TTL: Duration = Duration::from_secs(60);
const NEGATIVE_CACHE_CAP: usize = 1024;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("rusted/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("oauth introspection client builds")
    })
}

fn metadata_cache() -> &'static Mutex<HashMap<String, (Instant, Value)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn negative_cache() -> &'static Mutex<HashMap<[u8; 32], Instant>> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 32], Instant>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The oauth declaration off the stored metadata, if this function has one.
struct OauthDecl<'a> {
    issuer: &'a str,
    audience: &'a str,
    scopes: Vec<&'a str>,
    client_id_secret: Option<&'a str>,
    client_secret_secret: Option<&'a str>,
}

fn oauth(meta: &Value) -> Option<OauthDecl<'_>> {
    let auth = meta.get("auth")?;
    if auth.get("type")?.as_str()? != "oauth" {
        return None;
    }
    Some(OauthDecl {
        issuer: auth.get("issuer")?.as_str()?,
        audience: auth.get("audience")?.as_str()?,
        scopes: auth
            .get("scopes")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default(),
        client_id_secret: auth
            .get("introspectionClientIdSecret")
            .and_then(Value::as_str),
        client_secret_secret: auth
            .get("introspectionClientSecretSecret")
            .and_then(Value::as_str),
    })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
}

fn challenge(state: &AppState, name: &str) -> Response {
    let metadata = state.data_url(&format!("/.well-known/oauth-protected-resource/f/{name}"));
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": {
            "code": "unauthorized",
            "message": "authorize this MCP client with the resource owner"
        }})),
    )
        .into_response();
    response.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_str(&format!("Bearer resource_metadata=\"{metadata}\""))
            .expect("generated metadata URL is a valid header"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(axum::http::header::RETRY_AFTER, "10")],
        Json(json!({ "error": {
            "code": "auth_unavailable",
            "message": "the authorization server could not be reached; retry shortly"
        }})),
    )
        .into_response()
}

fn audience_matches(value: &Value, expected: &str) -> bool {
    value.as_str() == Some(expected)
        || value
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(expected)))
}

async fn json_response(mut response: reqwest::Response) -> Option<Value> {
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

/// Whether the SSRF vetting applies. The test/local escape hatch (also
/// honored by deploy-time issuer validation) permits loopback http issuers so
/// a mock authorization server can exist at all — the guard otherwise refuses
/// loopback by design.
async fn vet(url: &str) -> Result<(), String> {
    if std::env::var("RUSTED_OAUTH_ALLOW_HTTP").ok().as_deref() == Some("1") {
        return Ok(());
    }
    rusted_engine::outbound::vet_public_url(url).await
}

enum Introspection {
    /// Sanitized identity for a token the issuer accepted.
    Valid(Value),
    /// The issuer answered and judged the token unusable (inactive, wrong
    /// audience, expired, missing scopes…). Cacheable, challengeable.
    InvalidToken,
    /// The issuer could not be consulted: unreachable, redirecting,
    /// oversized, or answering something that is not its own metadata.
    Unavailable,
}

/// Authorization-server metadata for `issuer`, through the cache. `None`
/// means unavailable — a mismatched or unreadable document is not cached, so
/// an issuer misconfiguration heals as soon as the issuer does.
async fn server_metadata(issuer: &str) -> Option<Value> {
    if let Some((at, metadata)) = metadata_cache().lock().unwrap().get(issuer) {
        if at.elapsed() < METADATA_TTL {
            return Some(metadata.clone());
        }
    }
    let metadata_url = format!(
        "{}/.well-known/oauth-authorization-server",
        issuer.trim_end_matches('/')
    );
    vet(&metadata_url).await.ok()?;
    let response = client().get(metadata_url).send().await.ok()?;
    let metadata = json_response(response).await?;
    if metadata.get("issuer")?.as_str()? != issuer {
        return None;
    }
    let endpoint = metadata.get("introspection_endpoint")?.as_str()?;
    if !endpoint.starts_with(&format!("{}/", issuer.trim_end_matches('/'))) {
        return None;
    }
    let mut cache = metadata_cache().lock().unwrap();
    if cache.len() >= METADATA_CACHE_CAP {
        cache.clear();
    }
    cache.insert(issuer.to_string(), (Instant::now(), metadata.clone()));
    Some(metadata)
}

async fn introspect(
    decl: &OauthDecl<'_>,
    credentials: Option<(&str, &str)>,
    token: &str,
) -> Introspection {
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    if let Some(at) = negative_cache().lock().unwrap().get(&token_hash) {
        if at.elapsed() < NEGATIVE_TTL {
            return Introspection::InvalidToken;
        }
    }

    let Some(metadata) = server_metadata(decl.issuer).await else {
        return Introspection::Unavailable;
    };
    let endpoint = metadata
        .get("introspection_endpoint")
        .and_then(Value::as_str)
        .expect("checked when the metadata was cached");
    if vet(endpoint).await.is_err() {
        return Introspection::Unavailable;
    }
    let mut request = client()
        .post(endpoint)
        .form(&[("token", token), ("resource", decl.audience)]);
    if let Some((id, secret)) = credentials {
        // RFC 6749 §2.3.1 client_secret_basic — the interoperable default.
        request = request.basic_auth(id, Some(secret));
    }
    let Ok(response) = request.send().await else {
        return Introspection::Unavailable;
    };
    let Some(claims) = json_response(response).await else {
        return Introspection::Unavailable;
    };

    // From here the issuer has spoken: every refusal is a token verdict.
    let verdict = validate_claims(&claims, decl);
    if matches!(verdict, Introspection::InvalidToken) {
        let mut cache = negative_cache().lock().unwrap();
        if cache.len() >= NEGATIVE_CACHE_CAP {
            cache.clear();
        }
        cache.insert(token_hash, Instant::now());
    }
    verdict
}

fn validate_claims(claims: &Value, decl: &OauthDecl<'_>) -> Introspection {
    let active = claims.get("active").and_then(Value::as_bool);
    if active != Some(true) {
        return Introspection::InvalidToken;
    }
    if claims.get("iss").and_then(Value::as_str) != Some(decl.issuer) {
        return Introspection::InvalidToken;
    }
    let Some(aud) = claims.get("aud") else {
        return Introspection::InvalidToken;
    };
    if !audience_matches(aud, decl.audience) {
        return Introspection::InvalidToken;
    }
    match claims.get("exp").and_then(Value::as_u64) {
        Some(exp) if exp > crate::state::now_epoch() => {}
        _ => return Introspection::InvalidToken,
    }
    // An absent scope claim is an empty grant: fine when the resource
    // requires nothing, a refusal when it does.
    let scopes: Vec<&str> = claims
        .get("scope")
        .and_then(Value::as_str)
        .map(|raw| raw.split_ascii_whitespace().collect())
        .unwrap_or_default();
    if decl
        .scopes
        .iter()
        .any(|required| !scopes.contains(required))
    {
        return Introspection::InvalidToken;
    }
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let client_id = claims
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if subject.is_empty() || client_id.is_empty() {
        return Introspection::InvalidToken;
    }
    let mut verified = json!({
        "subject": subject,
        "clientId": client_id,
        "scopes": scopes,
    });
    if let Some(connection_id) = claims
        .get("connection_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        verified["connectionId"] = json!(connection_id);
    }
    Introspection::Valid(verified)
}

/// Authenticate one deployed MCP request. `Ok(None)` is the legacy
/// owner-key/public path; `Ok(Some(..))` is a verified external OAuth caller.
pub async fn authorize(
    state: &Arc<AppState>,
    fetched: &Fetched,
    name: &str,
    headers: &HeaderMap,
) -> Result<Option<Value>, Response> {
    let meta = fetched.mcp.as_ref().unwrap_or(&Value::Null);
    if let Some(decl) = oauth(meta) {
        // The token-less challenge is the protocol's opening move, not a
        // refusal — every legitimate flow starts with one, so it is not
        // recorded against the owner.
        let Some(token) = bearer(headers) else {
            return Err(challenge(state, name));
        };
        // Introspection happens before any sandbox and outside the plan's
        // outbound budget, so it gets its own gate against being used to
        // hammer the issuer. The owner's own rate applies.
        let (plan, _) = crate::api::plan_for_owner(state, fetched.owner).await;
        if state
            .rate_limiter
            .check(&format!("oauth:{name}"), plan.limits.rate_per_min)
            .is_err()
        {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": {
                    "code": "rate_limited",
                    "message": "too many authorization attempts; retry shortly"
                }})),
            )
                .into_response());
        }
        // Introspection credentials resolve from the vault per call and die
        // with it — never stored on a struct, never visible to JavaScript.
        let credentials = match (
            decl.client_id_secret,
            decl.client_secret_secret,
            fetched.owner,
        ) {
            (Some(id_name), Some(secret_name), Some(owner)) => {
                let names = [id_name.to_string(), secret_name.to_string()];
                match state.secrets.env_for(owner, &names).await {
                    Ok(resolved) => {
                        Some((resolved[&names[0]].clone(), resolved[&names[1]].clone()))
                    }
                    Err(detail) => {
                        record_refusal(
                            state,
                            name,
                            fetched.owner,
                            "error",
                            503,
                            format!("oauth introspection credentials: {detail}"),
                        );
                        return Err(unavailable());
                    }
                }
            }
            (Some(_), Some(_), None) => {
                record_refusal(
                    state,
                    name,
                    fetched.owner,
                    "error",
                    503,
                    "oauth introspection credentials: this function has no owner".to_string(),
                );
                return Err(unavailable());
            }
            _ => None,
        };
        return match introspect(
            &decl,
            credentials
                .as_ref()
                .map(|(id, s)| (id.as_str(), s.as_str())),
            token,
        )
        .await
        {
            Introspection::Valid(identity) => Ok(Some(identity)),
            Introspection::InvalidToken => {
                record_refusal(
                    state,
                    name,
                    fetched.owner,
                    "refused",
                    401,
                    "refused: oauth token rejected by the authorization server".to_string(),
                );
                Err(challenge(state, name))
            }
            Introspection::Unavailable => {
                record_refusal(
                    state,
                    name,
                    fetched.owner,
                    "error",
                    503,
                    "oauth: the authorization server could not be consulted".to_string(),
                );
                Err(unavailable())
            }
        };
    }

    let public = meta.get("public").and_then(Value::as_bool).unwrap_or(false);
    if public {
        return Ok(None);
    }
    let caller = match bearer(headers) {
        Some(token) => crate::auth::user_for_key(state, token).await,
        None => None,
    };
    if matches!((caller, fetched.owner), (Some(c), Some(o)) if c == o) {
        Ok(None)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({ "error": {
                "code": "unauthorized",
                "message": "this mcp function requires its owner's API key (Authorization: Bearer rk_live_…)"
            }})),
        )
            .into_response())
    }
}

pub async fn protected_resource(state: Arc<AppState>, name: String) -> Response {
    let fetched = match state.store.fetch(&name).await {
        Ok(Some(fetched)) if fetched.kind == "mcp" && fetched.published => fetched,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": { "code": "not_found" } })),
            )
                .into_response()
        }
    };
    let meta = fetched.mcp.as_ref().unwrap_or(&Value::Null);
    let Some(decl) = oauth(meta) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "code": "not_found" } })),
        )
            .into_response();
    };
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "resource": decl.audience,
            "authorization_servers": [decl.issuer],
            "bearer_methods_supported": ["header"],
            "scopes_supported": decl.scopes,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl<'a>() -> OauthDecl<'a> {
        OauthDecl {
            issuer: "https://app.example",
            audience: "https://api.example/f/notes",
            scopes: vec!["folders:read"],
            client_id_secret: None,
            client_secret_secret: None,
        }
    }

    fn claims(patch: Value) -> Value {
        let mut base = json!({
            "active": true,
            "iss": "https://app.example",
            "aud": "https://api.example/f/notes",
            "exp": crate::state::now_epoch() + 600,
            "scope": "folders:read folders:write",
            "sub": "github:42",
            "client_id": "client-1",
        });
        for (key, value) in patch.as_object().unwrap() {
            base[key] = value.clone();
        }
        base
    }

    #[test]
    fn audience_must_contain_the_exact_resource() {
        assert!(audience_matches(
            &json!("https://api.example/f/notes"),
            "https://api.example/f/notes"
        ));
        assert!(audience_matches(
            &json!(["https://api.example/f/other", "https://api.example/f/notes"]),
            "https://api.example/f/notes"
        ));
        assert!(!audience_matches(
            &json!("https://api.example/f/notes/extra"),
            "https://api.example/f/notes"
        ));
    }

    #[test]
    fn oauth_metadata_is_fail_closed() {
        let valid = json!({ "auth": {
            "type": "oauth",
            "issuer": "https://app.example",
            "audience": "https://api.example/f/notes",
            "scopes": ["folders:read"]
        }});
        let decl = oauth(&valid).expect("valid oauth meta parses");
        assert_eq!(decl.issuer, "https://app.example");
        assert_eq!(decl.audience, "https://api.example/f/notes");
        assert_eq!(decl.scopes, vec!["folders:read"]);
        assert!(decl.client_id_secret.is_none());
        assert!(oauth(&json!({ "auth": { "type": "api_key" } })).is_none());
        assert!(
            oauth(&json!({ "auth": { "type": "oauth", "issuer": "https://app.example" } }))
                .is_none()
        );
    }

    #[test]
    fn claim_validation_refuses_each_defect_and_accepts_the_clean_token() {
        assert!(matches!(
            validate_claims(&claims(json!({})), &decl()),
            Introspection::Valid(_)
        ));
        for defect in [
            json!({ "active": false }),
            json!({ "iss": "https://other.example" }),
            json!({ "aud": "https://api.example/f/other" }),
            json!({ "exp": 1 }),
            json!({ "scope": "folders:write" }),
            json!({ "sub": " " }),
            json!({ "client_id": "" }),
        ] {
            assert!(
                matches!(
                    validate_claims(&claims(defect.clone()), &decl()),
                    Introspection::InvalidToken
                ),
                "{defect} must refuse"
            );
        }
    }

    #[test]
    fn absent_scope_claim_passes_only_a_scopeless_requirement() {
        let mut no_scope = claims(json!({}));
        no_scope.as_object_mut().unwrap().remove("scope");
        assert!(matches!(
            validate_claims(&no_scope, &decl()),
            Introspection::InvalidToken
        ));
        let mut unscoped = decl();
        unscoped.scopes = Vec::new();
        match validate_claims(&no_scope, &unscoped) {
            Introspection::Valid(identity) => {
                assert_eq!(identity["scopes"], json!([]));
            }
            _ => panic!("a scopeless resource must accept a scopeless token"),
        }
    }
}
