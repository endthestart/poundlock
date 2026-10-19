//! The node-action writes commit without a disk flush; nothing else may.
use super::*;
use crate::crypto::SecretBox;
use std::cell::Cell;

thread_local! {
    /// How many writes on this thread took the relaxed path. Store calls are
    /// synchronous, so a test sees exactly its own.
    pub(super) static RELAXED_WRITES: Cell<usize> = const { Cell::new(0) };
}

// `PRAGMA synchronous` values.
const NORMAL: i64 = 1;
const FULL: i64 = 2;

fn open(directory: &std::path::Path) -> Store {
    let secrets = SecretBox::load_or_create(&directory.join("master-key")).unwrap();
    Store::open(&directory.join("state.sqlite3"), secrets).unwrap()
}

fn sync_mode(store: &Store) -> i64 {
    store
        .connection()
        .unwrap()
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .unwrap()
}

fn relaxed_writes() -> usize {
    RELAXED_WRITES.with(Cell::get)
}

fn enroll(store: &Store) -> String {
    store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "gpu".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap()
        .node_id
}

#[test]
fn a_new_store_syncs_fully_and_the_relaxed_mode_ends_with_its_guard() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    assert!(store.relaxed_sync_allowed, "the schema selects WAL");
    assert_eq!(sync_mode(&store), FULL);
    {
        let connection = store.connection().unwrap();
        let _relaxed = RelaxedSync::new(&connection, true);
        let inside: i64 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(inside, NORMAL);
    }
    assert_eq!(sync_mode(&store), FULL);
}

#[test]
fn node_action_writes_are_relaxed_and_leave_full_sync_behind() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    let node = enroll(&store);
    let payload = || json!({"runtime_profile":"coding","lease_id":"lease-1"});
    let before = relaxed_writes();

    let ensure = store
        .enqueue_action(&node, None, "ensure", payload())
        .unwrap();
    assert_eq!(relaxed_writes() - before, 1, "enqueue");
    assert_eq!(sync_mode(&store), FULL);

    assert!(store.poll_node_action(&node).unwrap().is_some());
    assert_eq!(relaxed_writes() - before, 2, "claim");
    assert_eq!(sync_mode(&store), FULL);

    assert!(
        store
            .complete_node_action(&node, &ensure.id, true, json!({}))
            .unwrap()
    );
    assert_eq!(relaxed_writes() - before, 3, "complete");
    assert_eq!(sync_mode(&store), FULL);

    // A failing write restores it too.
    assert!(
        store
            .enqueue_action("no-such-node", None, "ensure", payload())
            .is_err()
    );
    assert_eq!(sync_mode(&store), FULL);
}

#[test]
fn other_writes_keep_full_sync() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    let before = relaxed_writes();
    let node = enroll(&store);
    store
        .heartbeat_node(&node, "linux", &json!({"runtimes":[]}))
        .unwrap();
    store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into()],
        })
        .unwrap();
    store.set_node_maintenance(&node, None, true).unwrap();
    store.remove_node(&node).unwrap();
    assert_eq!(relaxed_writes(), before, "none of these is bookkeeping");
    assert_eq!(sync_mode(&store), FULL);
}

#[test]
fn nothing_is_relaxed_when_the_database_is_not_in_wal_mode() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = open(directory.path());
    let node = enroll(&store);
    store.relaxed_sync_allowed = false;
    let before = relaxed_writes();
    store
        .enqueue_action(&node, None, "ensure", json!({"lease_id":"lease-1"}))
        .unwrap();
    store.poll_node_action(&node).unwrap();
    assert_eq!(relaxed_writes(), before);
    assert_eq!(sync_mode(&store), FULL);
}

#[test]
fn relaxed_rows_are_kept_when_the_process_ends_normally() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    let node = enroll(&store);
    let action = store
        .enqueue_action(&node, None, "ensure", json!({"lease_id":"lease-1"}))
        .unwrap();
    drop(store);
    let reopened = open(directory.path());
    assert_eq!(reopened.action(&action.id).unwrap().unwrap().id, action.id);
}

fn enroll_as(store: &Store, token: String, name: &str) -> Result<EnrolledNode> {
    store.enroll_node_with_offline(
        &EnrollNode {
            token,
            name: name.into(),
            platform: "linux".into(),
            capabilities: json!({}),
        },
        // Every machine counts as offline, so only the token rules decide.
        -1_000_000,
    )
}

#[test]
fn an_enrollment_token_is_bound_to_its_machine_name() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    let id = enroll(&store);

    let unnamed = store.create_enrollment_token(15, None).unwrap();
    let error = enroll_as(&store, unnamed.clone(), "gpu").unwrap_err();
    assert!(
        error.to_string().contains("token created for its name"),
        "{error}"
    );
    // The refusal does not spend the token: it can still enroll a new machine.
    assert!(enroll_as(&store, unnamed, "laptop").is_ok());

    let for_laptop = store.create_enrollment_token(15, Some("laptop")).unwrap();
    let error = enroll_as(&store, for_laptop, "gpu").unwrap_err();
    assert!(
        error.to_string().contains("another machine name"),
        "{error}"
    );

    let for_gpu = store.create_enrollment_token(15, Some("gpu")).unwrap();
    assert_eq!(enroll_as(&store, for_gpu, "gpu").unwrap().node_id, id);
}

fn stored_key(store: &Store, id: &str) -> String {
    store
        .connection()
        .unwrap()
        .query_row(
            "SELECT api_key_ciphertext FROM endpoints WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn store_key(store: &Store, id: &str, value: &str) {
    store
        .connection()
        .unwrap()
        .execute(
            "UPDATE endpoints SET api_key_ciphertext=?1 WHERE id=?2",
            [value, id],
        )
        .unwrap();
}

fn add_endpoint(store: &Store, name: &str, key: &str) -> String {
    store
        .create_endpoint(&CreateEndpoint {
            name: name.into(),
            kind: "openai".into(),
            base_url: "http://localhost:8080/v1".into(),
            api_key: key.into(),
            managed_identity: Default::default(),
        })
        .unwrap()
        .id
}

#[test]
fn secrets_from_before_binding_are_re_sealed_once_and_cannot_be_swapped() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    let first = add_endpoint(&store, "first", "key-one");
    let second = add_endpoint(&store, "second", "key-two");
    // Write the first key the way 0.1.x did: sealed without a binding.
    let legacy = store.secrets.encrypt_unbound_for_test("key-one");
    store_key(&store, &first, &legacy);
    // As a 0.1.x database: no record that secrets were ever bound.
    store
        .connection()
        .unwrap()
        .execute("DELETE FROM store_meta", [])
        .unwrap();
    assert!(
        store.endpoint_secret(&first).is_err(),
        "unbound values never open"
    );
    drop(store);

    let store = open(directory.path());
    // The database as it was is kept for a rollback to 0.1.x.
    let snapshot = directory.path().join("state.sqlite3.pre-secret-binding");
    let before = rusqlite::Connection::open(&snapshot).unwrap();
    let kept: String = before
        .query_row(
            "SELECT api_key_ciphertext FROM endpoints WHERE id=?1",
            [&first],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kept, legacy);
    let resealed = stored_key(&store, &first);
    assert!(resealed.starts_with("v2."));
    assert_eq!(
        store.endpoint_secret(&first).unwrap().unwrap().api_key,
        "key-one"
    );
    drop(open(directory.path()));
    assert_eq!(stored_key(&store, &first), resealed, "re-sealing runs once");

    // A ciphertext copied into another row does not open there.
    store_key(&store, &second, &resealed);
    assert!(store.endpoint_secret(&second).is_err());

    // After the migration, an unbound value pasted in is refused, not re-sealed
    // under the row it was pasted into.
    let from_snapshot = store.secrets.encrypt_unbound_for_test("key-one");
    store_key(&store, &second, &from_snapshot);
    drop(store);
    let secrets =
        crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let refused = Store::open(&directory.path().join("state.sqlite3"), secrets)
        .err()
        .unwrap();
    assert!(
        refused.to_string().contains("sealed without their row"),
        "{refused}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&snapshot).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
