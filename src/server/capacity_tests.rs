use super::*;
use crate::domain::*;
use crate::store::Store;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn profile(workload: &str, node: Option<&str>, runtime: Option<&str>) -> ModelProfile {
    ModelProfile {
        id: "profile".into(),
        name: "coding".into(),
        aliases: Vec::new(),
        endpoint_id: "endpoint".into(),
        model_id: "model".into(),
        model_label: None,
        context_tokens: 131072,
        max_output_tokens: 8192,
        uses: crate::domain::resolve_uses(None, Some(workload))
            .unwrap()
            .unwrap(),
        workload_class: workload.into(),
        wake_policy: "on-demand".into(),
        idle_seconds: None,
        data_destination: "unknown".into(),

        data_destination_source: "profile".into(),
        thinking: "model_default".into(),
        thinking_budget_tokens: None,
        capabilities: Vec::new(),
        catalog: Default::default(),
        retirement: Default::default(),
        node_id: node.map(str::to_owned),
        runtime_profile: runtime.map(str::to_owned),
        created_at: "now".into(),
        last_check: ResourceCheck::default(),
    }
}

fn slot() -> SlotKey {
    Occupancy::exclusive_slot(&profile(
        "interactive-coding",
        Some("large-node"),
        Some("large-runtime"),
    ))
    .unwrap()
}

#[test]
fn completion_and_unplaced_profiles_are_not_exclusive() {
    assert!(
        Occupancy::exclusive_slot(&profile(
            "fast-code-completion",
            Some("small-node"),
            Some("prediction")
        ))
        .is_none()
    );
    assert!(Occupancy::exclusive_slot(&profile("interactive-coding", None, None)).is_none());
    assert!(
        Occupancy::exclusive_slot(&profile(
            "durable-coding-agent",
            Some("large-node"),
            Some("large-runtime")
        ))
        .is_some()
    );
}

#[test]
fn interactive_and_durable_share_the_same_node_runtime_slot() {
    let interactive = Occupancy::exclusive_slot(&profile(
        "interactive-coding",
        Some("large-node"),
        Some("large-runtime"),
    ));
    let durable = Occupancy::exclusive_slot(&profile(
        "durable-coding-agent",
        Some("large-node"),
        Some("large-runtime"),
    ));
    let other_node = Occupancy::exclusive_slot(&profile(
        "interactive-coding",
        Some("other-node"),
        Some("large-runtime"),
    ));
    assert_eq!(interactive, durable);
    assert_ne!(interactive, other_node);
}

#[test]
fn independent_gpu_runtimes_run_together_without_double_booking_either() {
    let occupancy = Occupancy::new();
    let first = Occupancy::exclusive_slot(&profile(
        "durable-coding-agent",
        Some("two-gpu-node"),
        Some("gpu-0"),
    ))
    .unwrap();
    let second = Occupancy::exclusive_slot(&profile(
        "durable-coding-agent",
        Some("two-gpu-node"),
        Some("gpu-1"),
    ))
    .unwrap();
    occupancy.try_claim_job(&first, "job-a").unwrap();
    occupancy.try_claim_job(&second, "job-b").unwrap();
    assert_eq!(
        occupancy.try_claim_job(&first, "job-c"),
        Err(Occupied::Job {
            job_id: "job-a".into()
        })
    );
    occupancy.release_job("job-a");
    occupancy.try_claim_job(&first, "job-c").unwrap();
    assert_eq!(
        occupancy.occupant(&second),
        Some(Occupant::Job {
            job_id: "job-b".into()
        })
    );
}

#[test]
fn second_interactive_is_rejected_without_waiting() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let _guard = occupancy
        .try_claim_interactive(slot.clone(), "lease-a")
        .unwrap();
    assert_eq!(
        occupancy.try_claim_interactive(slot, "lease-b").err(),
        Some(Occupied::Interactive)
    );
}

#[test]
fn interactive_is_rejected_while_a_job_holds_the_slot() {
    let occupancy = Occupancy::new();
    let slot = slot();
    occupancy.try_claim_job(&slot, "job-1").unwrap();
    assert_eq!(
        occupancy.try_claim_interactive(slot, "lease").err(),
        Some(Occupied::Job {
            job_id: "job-1".into()
        })
    );
}

#[test]
fn later_jobs_queue_and_do_not_steal_from_interactive() {
    let occupancy = Occupancy::new();
    let slot = slot();
    occupancy.try_claim_job(&slot, "job-1").unwrap();
    assert_eq!(
        occupancy.try_claim_job(&slot, "job-2"),
        Err(Occupied::Job {
            job_id: "job-1".into()
        })
    );
    occupancy.release_job("job-1");
    let _guard = occupancy
        .try_claim_interactive(slot.clone(), "live")
        .unwrap();
    assert_eq!(
        occupancy.try_claim_job(&slot, "job-2"),
        Err(Occupied::Interactive)
    );
}

#[test]
fn the_same_job_may_reclaim_across_prepare_ticks() {
    let occupancy = Occupancy::new();
    let slot = slot();
    occupancy.try_claim_job(&slot, "job-1").unwrap();
    occupancy.try_claim_job(&slot, "job-1").unwrap();
    occupancy.release_job("job-1");
    assert!(occupancy.occupant(&slot).is_none());
}

#[test]
fn interactive_guard_releases_from_drop() {
    let occupancy = Occupancy::new();
    let slot = slot();
    {
        let _guard = occupancy
            .try_claim_interactive(slot.clone(), "lease")
            .unwrap();
        assert!(occupancy.occupant(&slot).is_some());
        assert_eq!(occupancy.snapshot()[0].kind, "interactive");
        assert_eq!(occupancy.interactive_leases()[0].lease_id, "lease");
        assert_eq!(
            occupancy.interactive_leases()[0].runtime_profile,
            "large-runtime"
        );
    }
    assert!(occupancy.occupant(&slot).is_none());
    assert!(occupancy.snapshot().is_empty());
    assert!(occupancy.interactive_leases().is_empty());
}

#[test]
fn completion_profiles_do_not_hold_interactive_leases() {
    let occupancy = Occupancy::new();
    assert!(occupancy.interactive_leases().is_empty());
    assert!(
        Occupancy::exclusive_slot(&profile(
            "fast-code-completion",
            Some("small-node"),
            Some("prediction")
        ))
        .is_none()
    );
}

#[test]
fn recover_claims_preparing_and_running_jobs_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let secrets =
        crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    let token = store.create_enrollment_token(15, None).unwrap();
    let enrolled = store
        .enroll_node(&EnrollNode {
            token,
            name: "large-node".into(),
            platform: "linux".into(),
            capabilities: serde_json::json!({"runtimes":["large-runtime"]}),
        })
        .unwrap();
    let endpoint = store
        .create_endpoint(&CreateEndpoint {
            name: "endpoint".into(),
            kind: "openai".into(),
            base_url: "http://localhost:8080/v1".into(),
            api_key: "private-key".into(),
            managed_identity: Default::default(),
        })
        .unwrap();
    let model = store
        .create_model_profile(&CreateModelProfile {
            name: "coding-large".into(),
            aliases: Vec::new(),
            endpoint_id: endpoint.id,
            model_id: "upstream-large".into(),
            model_label: None,
            context_tokens: 131072,
            max_output_tokens: 8192,
            uses: None,
            workload_class: Some("durable-coding-agent".into()),
            wake_policy: "on-demand".into(),
            node_id: Some(enrolled.node_id),
            runtime_profile: Some("large-runtime".into()),
            idle_seconds: None,
            data_destination: "unknown".into(),

            thinking: "model_default".into(),
            thinking_budget_tokens: None,
            capabilities: Vec::new(),
        })
        .unwrap();
    let project = store
        .create_project(&CreateProject {
            name: "product".into(),
            roots: vec!["product".into()],
            instruction_files: Vec::new(),
        })
        .unwrap();
    let first = store
        .submit_job(&SubmitJob {
            source: None,
            idempotency_key: "first".into(),
            policy: Default::default(),
            project_id: project.id.clone(),
            model_profile_id: model.id.clone(),
            harness_profile_id: "pi".into(),
            prompt: "follow AGENTS.md".into(),
        })
        .unwrap();
    let second = store
        .submit_job(&SubmitJob {
            source: None,
            idempotency_key: "second".into(),
            policy: Default::default(),
            project_id: project.id,
            model_profile_id: model.id.clone(),
            harness_profile_id: "pi".into(),
            prompt: "stay queued".into(),
        })
        .unwrap();
    store
        .set_job_status(&first.id, "preparing", "runtime_starting", "preparing")
        .unwrap();
    store
        .set_job_status(&second.id, "preparing", "runtime_starting", "preparing")
        .unwrap();
    let occupancy = Occupancy::new();
    occupancy.recover(&store).unwrap();
    let slot = Occupancy::exclusive_slot(&model).unwrap();
    assert_eq!(
        occupancy.occupant(&slot),
        Some(Occupant::Job {
            job_id: first.id.clone()
        })
    );
    assert_eq!(
        occupancy.try_claim_job(&slot, &second.id),
        Err(Occupied::Job { job_id: first.id })
    );
}

fn wait(class: WaitClass, token: &str, session: Option<&str>, millis: u64) -> WaitRequest {
    WaitRequest {
        class,
        session: session.map(str::to_owned),
        token_id: token.into(),
        ticket: None,
        wait: Duration::from_millis(millis),
    }
}

/// Let spawned waiters run until each is parked in line.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_waiting_request_gets_the_slot_when_the_holder_lets_go() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let held = occupancy
        .try_claim_interactive(slot.clone(), "first")
        .unwrap();
    let waiter = {
        let (occupancy, slot) = (occupancy.clone(), slot.clone());
        tokio::spawn(async move {
            occupancy
                .claim_interactive(slot, "second", &wait(WaitClass::Worker, "t", None, 5_000))
                .await
                .map(|_guard| ())
        })
    };
    settle().await;
    assert_eq!(occupancy.queue_snapshot()[0].waiting.len(), 1);
    drop(held);
    assert_eq!(waiter.await.unwrap(), Ok(()));
}

#[tokio::test(flavor = "current_thread")]
async fn the_slot_goes_to_the_owner_before_earlier_work() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for (name, class) in [("worker", WaitClass::Worker), ("owner", WaitClass::Owner)] {
        let (occupancy, slot, order) = (occupancy.clone(), slot.clone(), order.clone());
        tasks.push(tokio::spawn(async move {
            let guard = occupancy
                .claim_interactive(slot, name, &wait(class, name, None, 5_000))
                .await
                .unwrap();
            order.lock().unwrap().push(name);
            tokio::task::yield_now().await;
            drop(guard);
        }));
        settle().await;
    }
    let classes: Vec<_> = occupancy.queue_snapshot()[0]
        .waiting
        .iter()
        .map(|waiter| waiter.class)
        .collect();
    assert_eq!(classes, ["owner", "worker"]);
    drop(held);
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(*order.lock().unwrap(), ["owner", "worker"]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_wait_that_runs_out_keeps_its_place_for_its_ticket() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let _held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    let Err(WaitError::Busy(first)) = occupancy
        .claim_interactive(slot.clone(), "a", &wait(WaitClass::Worker, "a", None, 50))
        .await
    else {
        panic!("the slot is held");
    };
    assert_eq!(first.position, 1);
    assert_eq!(first.holder_kind, Some("interactive"));
    let ticket = first
        .ticket
        .clone()
        .expect("a request that waited keeps a place");
    // Someone else joins the line behind the kept place.
    let Err(WaitError::Busy(second)) = occupancy
        .claim_interactive(slot.clone(), "b", &wait(WaitClass::Worker, "b", None, 50))
        .await
    else {
        panic!("the slot is held");
    };
    assert_eq!(second.position, 2);
    // Back with the ticket: still first. Another token cannot use it.
    let mut again = wait(WaitClass::Worker, "a", None, 50);
    again.ticket = Some(ticket.clone());
    let Err(WaitError::Busy(back)) = occupancy.claim_interactive(slot.clone(), "a", &again).await
    else {
        panic!("the slot is held");
    };
    assert_eq!(
        (back.position, back.ticket.as_deref()),
        (1, Some(ticket.as_str()))
    );
    let mut stolen = wait(WaitClass::Worker, "c", None, 0);
    stolen.ticket = Some(ticket);
    let Err(WaitError::Busy(other)) = occupancy.claim_interactive(slot, "c", &stolen).await else {
        panic!("the slot is held");
    };
    assert_eq!((other.position, other.ticket), (3, None));
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_that_goes_away_leaves_the_line() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let _held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    let waiter = {
        let (occupancy, slot) = (occupancy.clone(), slot.clone());
        tokio::spawn(async move {
            let _ = occupancy
                .claim_interactive(slot, "gone", &wait(WaitClass::Worker, "t", None, 5_000))
                .await;
        })
    };
    settle().await;
    assert_eq!(occupancy.queue_snapshot()[0].waiting.len(), 1);
    waiter.abort();
    let _ = waiter.await;
    assert!(occupancy.queue_snapshot()[0].waiting.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn one_token_may_keep_at_most_four_waiting() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let _held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    let mut tasks = Vec::new();
    for index in 0..MAX_WAITERS_PER_TOKEN {
        let (occupancy, slot) = (occupancy.clone(), slot.clone());
        tasks.push(tokio::spawn(async move {
            let _ = occupancy
                .claim_interactive(
                    slot,
                    format!("w{index}"),
                    &wait(WaitClass::Worker, "t", None, 5_000),
                )
                .await;
        }));
    }
    settle().await;
    assert_eq!(
        occupancy
            .claim_interactive(
                slot.clone(),
                "fifth",
                &wait(WaitClass::Worker, "t", None, 5_000)
            )
            .await
            .err(),
        Some(WaitError::Full { per_token: true })
    );
    for task in tasks {
        task.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_freed_slot_is_held_for_the_waiter_that_outranks_a_newcomer() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    let owner = {
        let (occupancy, slot) = (occupancy.clone(), slot.clone());
        tokio::spawn(async move {
            occupancy
                .claim_interactive(slot, "owner", &wait(WaitClass::Owner, "o", None, 5_000))
                .await
                .map(|_guard| ())
        })
    };
    settle().await;
    drop(held);
    // The owner has not run yet: a request that does not wait cannot take its slot.
    assert_eq!(
        occupancy
            .try_claim_interactive(slot.clone(), "newcomer")
            .err(),
        Some(Occupied::Reserved)
    );
    assert_eq!(
        occupancy.try_claim_job(&slot, "job"),
        Err(Occupied::Reserved)
    );
    assert_eq!(owner.await.unwrap(), Ok(()));
}

#[tokio::test(flavor = "current_thread")]
async fn the_session_that_just_let_go_comes_back_first() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let held = occupancy
        .claim_interactive(
            slot.clone(),
            "a-1",
            &wait(WaitClass::Worker, "a", Some("A"), 0),
        )
        .await
        .unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for (name, session) in [("b", "B"), ("a", "A")] {
        let (occupancy, slot, order) = (occupancy.clone(), slot.clone(), order.clone());
        tasks.push(tokio::spawn(async move {
            let guard = occupancy
                .claim_interactive(
                    slot,
                    name,
                    &wait(WaitClass::Worker, name, Some(session), 5_000),
                )
                .await
                .unwrap();
            order.lock().unwrap().push(name);
            tokio::task::yield_now().await;
            drop(guard);
        }));
        settle().await;
    }
    drop(held);
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(*order.lock().unwrap(), ["a", "b"]);
}

#[test]
fn waiting_raises_a_class_one_step_a_minute() {
    assert_eq!(rank(WaitClass::Background, Duration::ZERO), 3);
    assert_eq!(rank(WaitClass::Background, Duration::from_secs(61)), 2);
    assert_eq!(rank(WaitClass::Background, Duration::from_secs(400)), 0);
    assert_eq!(rank(WaitClass::Owner, Duration::from_secs(400)), 0);
}

#[test]
fn a_job_waits_in_line_and_a_stopped_job_leaves_it() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let held = occupancy
        .try_claim_interactive(slot.clone(), "live")
        .unwrap();
    assert_eq!(
        occupancy.try_claim_job(&slot, "job-1"),
        Err(Occupied::Interactive)
    );
    assert_eq!(occupancy.queue_snapshot()[0].waiting[0].kind, "job");
    occupancy.release_job("job-1");
    assert!(occupancy.queue_snapshot()[0].waiting.is_empty());
    drop(held);
    occupancy.try_claim_job(&slot, "job-1").unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn jobs_and_kept_places_do_not_fill_the_line() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let _held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    for index in 0..MAX_WAITERS_PER_SLOT {
        let _ = occupancy.try_claim_job(&slot, &format!("job-{index}"));
    }
    // Sixteen queued jobs: a request still waits rather than hearing the line is full.
    assert!(matches!(
        occupancy
            .claim_interactive(slot.clone(), "r", &wait(WaitClass::Worker, "t", None, 20))
            .await,
        Err(WaitError::Busy(_))
    ));
    // Four kept places of one token do not count as four waiting.
    for index in 0..MAX_WAITERS_PER_TOKEN {
        let _ = occupancy
            .claim_interactive(
                slot.clone(),
                format!("k{index}"),
                &wait(WaitClass::Worker, "t", None, 10),
            )
            .await;
    }
    let Err(WaitError::Busy(place)) = occupancy
        .claim_interactive(slot, "fifth", &wait(WaitClass::Worker, "t", None, 10))
        .await
    else {
        panic!("a fifth request waits");
    };
    assert!(place.ticket.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn a_ticket_another_request_waits_on_is_not_shared() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let _held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    let Err(WaitError::Busy(kept)) = occupancy
        .claim_interactive(slot.clone(), "a", &wait(WaitClass::Worker, "t", None, 10))
        .await
    else {
        panic!("the slot is held");
    };
    let ticket = kept.ticket.unwrap();
    let mut again = wait(WaitClass::Worker, "t", None, 5_000);
    again.ticket = Some(ticket.clone());
    let first = {
        let (occupancy, slot, again) = (occupancy.clone(), slot.clone(), again.clone());
        tokio::spawn(async move { occupancy.claim_interactive(slot, "b", &again).await.is_ok() })
    };
    settle().await;
    // The same ticket again, while it is in use: a place of its own, not a share.
    let second = {
        let (occupancy, slot) = (occupancy.clone(), slot.clone());
        tokio::spawn(async move { occupancy.claim_interactive(slot, "c", &again).await.is_ok() })
    };
    settle().await;
    assert_eq!(occupancy.queue_snapshot()[0].waiting.len(), 2);
    first.abort();
    second.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn a_stopped_job_wakes_the_request_it_was_keeping_out() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let held = occupancy
        .try_claim_interactive(slot.clone(), "holder")
        .unwrap();
    let _ = occupancy.try_claim_job(&slot, "old-job");
    // The job has waited long enough to outrank ordinary work.
    for waiter in &mut occupancy.lock().queues.get_mut(&slot).unwrap().waiters {
        waiter.enqueued = Instant::now() - Duration::from_secs(600);
    }
    let request = {
        let (occupancy, slot) = (occupancy.clone(), slot.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            occupancy
                .claim_interactive(slot, "r", &wait(WaitClass::Worker, "t", None, 5_000))
                .await
                .map(|_guard| started.elapsed())
        })
    };
    settle().await;
    drop(held);
    settle().await;
    occupancy.release_job("old-job");
    let waited = request.await.unwrap().unwrap();
    assert!(waited < Duration::from_millis(500), "{waited:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_is_never_missed_by_the_next_in_line() {
    let occupancy = Occupancy::new();
    let slot = slot();
    let started = Instant::now();
    let mut tasks = Vec::new();
    for index in 0..4 {
        let (occupancy, slot) = (occupancy.clone(), slot.clone());
        tasks.push(tokio::spawn(async move {
            let mut slowest = Duration::ZERO;
            for round in 0..150 {
                let asked = Instant::now();
                let guard = occupancy
                    .claim_interactive(
                        slot.clone(),
                        format!("{index}-{round}"),
                        &wait(WaitClass::Worker, &format!("t{index}"), None, 10_000),
                    )
                    .await
                    .unwrap();
                slowest = slowest.max(asked.elapsed());
                drop(guard);
                tokio::task::yield_now().await;
            }
            slowest
        }));
    }
    for task in tasks {
        let slowest = task.await.unwrap();
        assert!(
            slowest < Duration::from_millis(500),
            "a waiter stalled {slowest:?}"
        );
    }
    assert!(started.elapsed() < Duration::from_secs(10));
}
