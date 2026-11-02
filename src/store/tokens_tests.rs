use crate::crypto::SecretBox;
use crate::domain::CreateApiToken;
use crate::store::Store;
use std::time::{Duration, Instant};

#[test]
fn only_poundlocks_own_check_principal_is_marked_internal() {
    let directory = tempfile::tempdir().unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into()],
        })
        .unwrap();
    let internal = store.operator_check_principal().unwrap();
    let listed = store.list_api_tokens().unwrap();
    assert!(
        listed
            .iter()
            .find(|t| t.id == internal.id)
            .unwrap()
            .internal
    );
    assert!(!listed.iter().find(|t| t.name == "client").unwrap().internal);
}

#[test]
fn a_token_check_does_not_wait_behind_a_write() {
    let directory = tempfile::tempdir().unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    let issued = store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["inference:invoke".into()],
        })
        .unwrap();
    // The first check records last use; the next ones only read.
    assert!(
        store
            .authenticate_api_token(&issued.token, Some("inference:invoke"))
            .unwrap()
            .is_some()
    );
    let held = store.connection().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let checker = store.clone();
    let token = issued.token.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        let found = checker
            .authenticate_api_token(&token, Some("inference:invoke"))
            .unwrap();
        sender.send((found.is_some(), started.elapsed())).unwrap();
    });
    let (found, elapsed) = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("the token check waited for the held writer");
    drop(held);
    assert!(found, "the token was not recognised");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}
