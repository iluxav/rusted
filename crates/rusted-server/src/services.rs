//! What the host lends a running handler beyond `fetch`: inbox reads, durable
//! state, and object-storage bindings, all behind [`rusted_engine::HostServices`].
//!
//! Built per invocation with the owner, the stable function name, the declared
//! bindings, and the plan's allowances baked in — nothing here trusts anything
//! the request said. Ad-hoc runs get no instance at all, so every capability
//! is absent rather than reachable-but-unauthorized.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::fnstate::{CasOutcome, StateAllowance};
use crate::state::AppState;

pub struct OwnerScopedServices {
    state: Arc<AppState>,
    user_id: Uuid,
    /// The stable function name state and objects are scoped by. Ad-hoc
    /// invocations have none — and no capabilities either.
    function_name: String,
    /// Declared bindings, from the stored record.
    objects: BTreeMap<String, rusted_engine::ObjectBinding>,
    allowance: StateAllowance,
}

impl OwnerScopedServices {
    pub fn new(
        state: Arc<AppState>,
        user_id: Uuid,
        function_name: String,
        objects: BTreeMap<String, rusted_engine::ObjectBinding>,
        allowance: StateAllowance,
    ) -> Self {
        Self {
            state,
            user_id,
            function_name,
            objects,
            allowance,
        }
    }
}

/// The state ops the glue sends. `expectedVersion` distinguishes "absent"
/// (a bug worth naming) from `null` (create-only), so it arrives as a raw
/// JSON value rather than an `Option<i64>` that would flatten the two.
#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
pub(crate) enum StateOp {
    #[serde(rename = "get")]
    Get { key: String },
    #[serde(rename = "cas", rename_all = "camelCase")]
    Cas {
        key: String,
        // Plain `Value`, not `Option<Value>`: serde folds JSON null into
        // `None`, and null is exactly the meaningful create-only marker.
        #[serde(default)]
        expected_version: Value,
        #[serde(default)]
        value: Value,
    },
    #[serde(rename = "delete", rename_all = "camelCase")]
    Delete {
        key: String,
        #[serde(default)]
        expected_version: Value,
    },
    #[serde(rename = "list")]
    List {
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        cursor: String,
        #[serde(default)]
        limit: Option<i64>,
    },
}

async fn run_state_op(services: &OwnerScopedServices, op_json: String) -> Result<String, String> {
    let op: StateOp = serde_json::from_str(&op_json).map_err(|e| format!("malformed op: {e}"))?;
    let store = &services.state.fnstate;
    let (user, function) = (services.user_id, services.function_name.as_str());
    let result = match op {
        StateOp::Get { key } => match store.get(user, function, &key).await? {
            Some(entry) => json!({ "entry": entry }),
            None => json!({ "entry": null }),
        },
        StateOp::Cas {
            key,
            expected_version,
            value,
        } => {
            let expected = parse_expected(&expected_version)?;
            match store
                .compare_and_set(user, function, &key, expected, &value, services.allowance)
                .await?
            {
                CasOutcome::Applied { version } => json!({ "ok": true, "version": version }),
                CasOutcome::Conflict { current_version } => {
                    json!({ "ok": false, "currentVersion": current_version })
                }
            }
        }
        StateOp::Delete {
            key,
            expected_version,
        } => {
            let expected = parse_expected(&expected_version)?
                .ok_or_else(|| "delete needs the expected version".to_string())?;
            match store.delete(user, function, &key, expected).await? {
                CasOutcome::Applied { .. } => json!({ "ok": true }),
                CasOutcome::Conflict { current_version } => {
                    json!({ "ok": false, "currentVersion": current_version })
                }
            }
        }
        StateOp::List {
            prefix,
            cursor,
            limit,
        } => {
            let (items, next) = store
                .list(user, function, &prefix, &cursor, limit.unwrap_or(100))
                .await?;
            match next {
                Some(cursor) => json!({ "items": items, "cursor": cursor }),
                None => json!({ "items": items }),
            }
        }
    };
    Ok(result.to_string())
}

/// `null` (or an absent field — JavaScript's undefined) means create-only; a
/// number means that exact version; anything else is the caller's bug, named.
pub(crate) fn parse_expected(raw: &Value) -> Result<Option<i64>, String> {
    match raw {
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| "expectedVersion must be an integer".to_string()),
        _ => Err("expectedVersion must be a number or null".to_string()),
    }
}

async fn run_object_op(
    services: &OwnerScopedServices,
    binding_name: String,
    op_json: String,
) -> Result<String, String> {
    let Some(binding) = services.objects.get(&binding_name) else {
        return Err(format!("no object binding named {binding_name}"));
    };
    // Credentials resolve through the vault's cache per call and die with it —
    // they never sit in this struct, let alone reach JavaScript.
    let names = [
        binding.access_key_id_secret.clone(),
        binding.secret_access_key_secret.clone(),
    ];
    let credentials = services
        .state
        .secrets
        .env_for(services.user_id, &names)
        .await?;
    let namespace = crate::objects::namespace(services.user_id, &services.function_name);
    services
        .state
        .objects
        .perform(
            binding,
            &credentials[&names[0]],
            &credentials[&names[1]],
            &namespace,
            &op_json,
        )
        .await
}

impl rusted_engine::HostServices for OwnerScopedServices {
    fn inbox_get(
        &self,
        name: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            match crate::inbox::read(&self.state, self.user_id, &name).await {
                Ok(crate::inbox::Reading::Alive { messages, .. }) => {
                    Ok(serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into()))
                }
                // A handler gets the same answer a person does: gone is gone,
                // and it cannot tell that apart from never having existed.
                Ok(crate::inbox::Reading::Gone) => Err(format!(
                    "inbox '{name}' has expired, been drained, or never existed"
                )),
                Err(e) => Err(e),
            }
        })
    }

    fn state_op(
        &self,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(run_state_op(self, op_json))
    }

    fn seal_op(
        &self,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let op = crate::secrets::SealOp::parse(&op_json)?;
            // The key secret is resolved from the vault and used here — it
            // never has to be declared in config.secrets, so the key material
            // can stay entirely outside JavaScript.
            let names = [op.key_secret().to_string()];
            let material = self.state.secrets.env_for(self.user_id, &names).await?;
            op.perform(&material[&names[0]])
        })
    }

    fn object_op(
        &self,
        binding: String,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(run_object_op(self, binding, op_json))
    }
}
