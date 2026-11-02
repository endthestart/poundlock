use crate::crypto::SecretBox;
use crate::store::Store;
use std::time::{Duration, Instant};

fn store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    (directory, store)
}

/// The Hub's console polled the job list every 4 s, and each list held the writer about 2 s (30 Oct).
#[test]
fn a_job_list_does_not_wait_behind_a_write() {
    let (_directory, store) = store();
    let held = store.connection().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let lister = store.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        let listed = lister
            .list_jobs_for(100, Some("token"))
            .map(|jobs| jobs.len());
        sender.send((listed.unwrap(), started.elapsed())).unwrap();
    });
    let (listed, elapsed) = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("the job list waited for the held writer");
    drop(held);
    assert_eq!(listed, 0);
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[test]
fn job_lists_read_by_the_created_at_indexes() {
    let (_directory, store) = store();
    let connection = store.reader().unwrap();
    for (query, index) in [
        (
            "SELECT id FROM jobs ORDER BY created_at DESC LIMIT 100",
            "jobs_created",
        ),
        (
            "SELECT id FROM jobs WHERE api_token_id='t' ORDER BY created_at DESC LIMIT 100",
            "jobs_token_created",
        ),
    ] {
        let plan: Vec<String> = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(plan.contains(index), "{query}: {plan}");
        assert!(
            !plan.contains("TEMP B-TREE"),
            "{query} sorts every row: {plan}"
        );
    }
}
