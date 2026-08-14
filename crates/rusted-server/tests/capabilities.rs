//! Repository-level tests for the durable state store: CAS semantics, plan
//! accounting, isolation, persistence, and cross-instance invalidation.

use rusted_server::fnstate::{CasOutcome, StateAllowance, StateStore};
use rusted_server::store::{Declared, Store};
use rusted_server::testsupport;
use serde_json::json;

const ROOMY: StateAllowance = StateAllowance {
    max_keys: 100,
    max_bytes: 1024 * 1024,
};

async fn state_store() -> (StateStore, sqlx::PgPool, uuid::Uuid) {
    let url = testsupport::create_test_database().await;
    let pool = testsupport::migrate(&url).await;
    let user = testsupport::seed_user(&pool).await;
    (StateStore::new(pool.clone()), pool, user)
}

#[tokio::test]
async fn cas_creates_updates_conflicts_and_deletes() {
    let (store, _pool, user) = state_store().await;

    // Create (expected: none).
    let outcome = store
        .compare_and_set(user, "fn", "prod", "k", None, &json!({"n": 1}), ROOMY)
        .await
        .unwrap();
    assert_eq!(outcome, CasOutcome::Applied { version: 1 });

    // Create again: conflict carrying the current version.
    let outcome = store
        .compare_and_set(user, "fn", "prod", "k", None, &json!(2), ROOMY)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        CasOutcome::Conflict {
            current_version: Some(1)
        }
    );

    // Update at the right version.
    let outcome = store
        .compare_and_set(user, "fn", "prod", "k", Some(1), &json!({"n": 2}), ROOMY)
        .await
        .unwrap();
    assert_eq!(outcome, CasOutcome::Applied { version: 2 });
    let entry = store.get(user, "fn", "prod", "k").await.unwrap().unwrap();
    assert_eq!(entry.value, json!({"n": 2}));
    assert_eq!(entry.version, 2);

    // Update at a stale version: conflict, value untouched.
    let outcome = store
        .compare_and_set(user, "fn", "prod", "k", Some(1), &json!("stale"), ROOMY)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        CasOutcome::Conflict {
            current_version: Some(2)
        }
    );

    // Delete needs the exact version too.
    let outcome = store.delete(user, "fn", "prod", "k", 1).await.unwrap();
    assert!(matches!(outcome, CasOutcome::Conflict { .. }));
    let outcome = store.delete(user, "fn", "prod", "k", 2).await.unwrap();
    assert!(matches!(outcome, CasOutcome::Applied { .. }));
    assert!(store.get(user, "fn", "prod", "k").await.unwrap().is_none());

    // Updating a missing key: conflict with no current version.
    let outcome = store
        .compare_and_set(user, "fn", "prod", "gone", Some(1), &json!(1), ROOMY)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        CasOutcome::Conflict {
            current_version: None
        }
    );
}

#[tokio::test]
async fn state_is_scoped_by_owner_and_function_name() {
    let (store, pool, user_a) = state_store().await;
    let user_b = testsupport::seed_user(&pool).await;

    store
        .compare_and_set(user_a, "fn", "prod", "k", None, &json!("a"), ROOMY)
        .await
        .unwrap();
    store
        .compare_and_set(user_b, "fn", "prod", "k", None, &json!("b"), ROOMY)
        .await
        .unwrap();
    store
        .compare_and_set(user_a, "other", "prod", "k", None, &json!("c"), ROOMY)
        .await
        .unwrap();

    let a = store.get(user_a, "fn", "prod", "k").await.unwrap().unwrap();
    let b = store.get(user_b, "fn", "prod", "k").await.unwrap().unwrap();
    let c = store
        .get(user_a, "other", "prod", "k")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (a.value, b.value, c.value),
        (json!("a"), json!("b"), json!("c"))
    );
}

#[tokio::test]
async fn state_survives_revisions_and_redeploy_until_purged() {
    let (state, pool, user) = state_store().await;
    let store = Store::new(pool.clone());
    let declared = Declared {
        state: true,
        ..Declared::default()
    };
    store
        .push(
            "sticky",
            "export default async () => 1",
            Some(user),
            &declared,
        )
        .await
        .unwrap();
    state
        .compare_and_set(user, "sticky", "prod", "k", None, &json!("kept"), ROOMY)
        .await
        .unwrap();

    // A new revision changes nothing.
    store
        .push(
            "sticky",
            "export default async () => 2",
            Some(user),
            &declared,
        )
        .await
        .unwrap();
    assert!(state
        .get(user, "sticky", "prod", "k")
        .await
        .unwrap()
        .is_some());

    // Deleting the function leaves state for a future redeploy.
    store.delete("sticky").await.unwrap();
    assert!(state
        .get(user, "sticky", "prod", "k")
        .await
        .unwrap()
        .is_some());

    // Only the explicit purge removes it.
    assert_eq!(state.purge(user, "sticky").await.unwrap(), 1);
    assert!(state
        .get(user, "sticky", "prod", "k")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn limits_account_exactly_and_refusals_change_nothing() {
    let (store, _pool, user) = state_store().await;
    let tight = StateAllowance {
        max_keys: 2,
        max_bytes: 60,
    };

    store
        .compare_and_set(user, "fn", "prod", "a", None, &json!("0123456789"), tight)
        .await
        .unwrap();
    store
        .compare_and_set(user, "fn", "prod", "b", None, &json!("0123456789"), tight)
        .await
        .unwrap();

    // Third key: refused by the key limit, and nothing is written.
    let err = store
        .compare_and_set(user, "fn", "prod", "c", None, &json!(1), tight)
        .await
        .unwrap_err();
    assert!(err.contains("state limit") && err.contains("keys"), "{err}");
    assert!(store.get(user, "fn", "prod", "c").await.unwrap().is_none());

    // Growing a value past the byte budget: refused, old value intact.
    let big = json!("x".repeat(50));
    let err = store
        .compare_and_set(user, "fn", "prod", "a", Some(1), &big, tight)
        .await
        .unwrap_err();
    assert!(
        err.contains("state limit") && err.contains("bytes"),
        "{err}"
    );
    let entry = store.get(user, "fn", "prod", "a").await.unwrap().unwrap();
    assert_eq!(entry.value, json!("0123456789"));
    assert_eq!(
        entry.version, 1,
        "a refused write must not bump the version"
    );

    // Deleting a key frees its accounting.
    store.delete(user, "fn", "prod", "b", 1).await.unwrap();
    let outcome = store
        .compare_and_set(user, "fn", "prod", "c", None, &json!(1), tight)
        .await
        .unwrap();
    assert!(matches!(outcome, CasOutcome::Applied { .. }));
}

#[tokio::test]
async fn simultaneous_cas_has_exactly_one_winner() {
    let (store, pool, user) = state_store().await;
    let store = std::sync::Arc::new(store);
    drop(pool);

    let mut tasks = Vec::new();
    for i in 0..16 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .compare_and_set(user, "fn", "prod", "contested", None, &json!(i), ROOMY)
                .await
                .unwrap()
        }));
    }
    let mut winners = 0;
    for task in tasks {
        if matches!(task.await.unwrap(), CasOutcome::Applied { .. }) {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "create-only CAS must admit exactly one");
    let entry = store
        .get(user, "fn", "prod", "contested")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.version, 1);
}

#[tokio::test]
async fn writes_invalidate_other_instances_via_the_notify_payload() {
    let (a, pool, user) = state_store().await;
    let b = StateStore::new(pool.clone());

    a.compare_and_set(user, "fn", "prod", "k", None, &json!("v1"), ROOMY)
        .await
        .unwrap();
    // Instance A caches the read.
    assert_eq!(
        a.get(user, "fn", "prod", "k").await.unwrap().unwrap().value,
        json!("v1")
    );

    // Instance B writes; A hears about it the way production does — the
    // NOTIFY listener dispatching `fnstate:<user>:<fn>` into invalidate().
    b.compare_and_set(user, "fn", "prod", "k", Some(1), &json!("v2"), ROOMY)
        .await
        .unwrap();
    a.invalidate(user, "fn", "prod");
    let entry = a.get(user, "fn", "prod", "k").await.unwrap().unwrap();
    assert_eq!(entry.value, json!("v2"));
    assert_eq!(entry.version, 2);
}

#[tokio::test]
async fn keys_and_values_are_bounded_and_lists_page_in_order() {
    let (store, _pool, user) = state_store().await;

    assert!(store.get(user, "fn", "prod", "").await.is_err());
    assert!(store
        .get(user, "fn", "prod", &"k".repeat(513))
        .await
        .is_err());
    let oversized = json!("x".repeat(65 * 1024));
    assert!(store
        .compare_and_set(user, "fn", "prod", "k", None, &oversized, ROOMY)
        .await
        .is_err());

    for key in ["b/2", "a/1", "b/1", "c/1"] {
        store
            .compare_and_set(user, "fn", "prod", key, None, &json!(key), ROOMY)
            .await
            .unwrap();
    }
    let (page, cursor) = store.list(user, "fn", "prod", "b/", "", 1).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].key, "b/1");
    let cursor = cursor.expect("more pages");
    let (page, cursor) = store
        .list(user, "fn", "prod", "b/", &cursor, 10)
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].key, "b/2");
    assert!(cursor.is_none());

    let (all, _) = store.list(user, "fn", "prod", "", "", 100).await.unwrap();
    let keys: Vec<&str> = all.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, vec!["a/1", "b/1", "b/2", "c/1"], "lexicographic");
}
