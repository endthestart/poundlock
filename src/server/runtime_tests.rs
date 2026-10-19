use super::*;
use crate::client::InferenceClient;
use crate::config::AppConfig;
use crate::domain::{CreateEndpoint, CreateModelProfile, EnrollNode, ModelProfile};
use crate::server::capacity::Occupancy;
use crate::store::Store;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct Fixture {
    _directory: tempfile::TempDir,
    state: AppState,
}

impl Fixture {
    fn new(external_timeout_seconds: u64) -> Self {
        Self::with_offline_threshold(external_timeout_seconds, 15)
    }

    fn with_offline_threshold(external_timeout_seconds: u64, node_offline_seconds: i64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let secrets =
            crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
        let store =
            Store::open(&directory.path().join("poundlock.sqlite3"), secrets.clone()).unwrap();
        let config = AppConfig {
            allow_job_network: false,
            wake_socket: None,
            git_hosts: vec!["github.com".into()],
            bind: "127.0.0.1:0".parse().unwrap(),
            data_dir: directory.path().join("data"),
            runtime_dir: directory.path().join("runtime"),
            projects_dir: directory.path().join("projects"),
            work_dir: directory.path().join("work"),
            admin_token: "admin".into(),
            runner_token: "runner".into(),
            secrets,
            external_timeout_seconds,
            job_inference_timeout_seconds: 30,
            node_offline_seconds,
            client_artifacts_dir: None,
            max_active_jobs_per_token: 10,
            max_requests_in_flight: 512,
            capacity_default_wait_seconds: 0,
            openrouter_catalog: crate::config::CatalogMode::OnRequest,
            backup: None,
        };
        let state = AppState {
            config: Arc::new(config),
            managed_identities: crate::litellm::ManagedIdentityManager::new(store.clone()).unwrap(),
            store,
            inference: InferenceClient::new(external_timeout_seconds).unwrap(),
            occupancy: Occupancy::new(),
            runners: Default::default(),
        };
        Self {
            _directory: directory,
            state,
        }
    }

    fn profile(&self, base_url: &str, wake_policy: &str, attach_node: bool) -> ModelProfile {
        let endpoint = self
            .state
            .store
            .create_endpoint(&CreateEndpoint {
                name: uuid::Uuid::new_v4().to_string(),
                kind: "openai".into(),
                base_url: base_url.into(),
                api_key: String::new(),
                managed_identity: Default::default(),
            })
            .unwrap();
        let node_id = attach_node.then(|| {
            let token = self.state.store.create_enrollment_token(15, None).unwrap();
            self.state
                .store
                .enroll_node(&EnrollNode {
                    token,
                    name: uuid::Uuid::new_v4().to_string(),
                    platform: "linux".into(),
                    capabilities: serde_json::json!({"runtimes":["coding"]}),
                })
                .unwrap()
                .node_id
        });
        self.state
            .store
            .create_model_profile(&CreateModelProfile {
                name: uuid::Uuid::new_v4().to_string(),
                aliases: Vec::new(),
                endpoint_id: endpoint.id,
                model_id: "coding-model".into(),
                model_label: None,
                context_tokens: 32768,
                max_output_tokens: 4096,
                uses: None,
                workload_class: Some("interactive-coding".into()),
                wake_policy: wake_policy.into(),
                node_id,
                runtime_profile: attach_node.then(|| "coding".into()),
                idle_seconds: None,
                data_destination: "unknown".into(),
                thinking: "model_default".into(),
                thinking_budget_tokens: None,
                capabilities: Vec::new(),
            })
            .unwrap()
    }
}

#[tokio::test]
async fn no_node_relays_without_a_readiness_probe() {
    let fixture = Fixture::new(1);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "on-demand", false);
    let mut lease = RuntimeLease::new(fixture.state.clone(), profile.clone(), "direct".into());

    tokio::time::timeout(
        Duration::from_millis(100),
        prepare_profile(&fixture.state, &mut lease),
    )
    .await
    .expect("no-node preparation must not perform network I/O")
    .unwrap();
}

#[tokio::test]
async fn never_wake_enqueues_keep_awake_without_a_readiness_probe() {
    let fixture = Fixture::new(1);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "never", true);
    let node_id = profile.node_id.clone().unwrap();
    let mut lease = RuntimeLease::new(
        fixture.state.clone(),
        profile.clone(),
        "always-ready".into(),
    );

    tokio::time::timeout(
        Duration::from_millis(100),
        prepare_profile(&fixture.state, &mut lease),
    )
    .await
    .expect("never-wake preparation must not perform network I/O")
    .unwrap();
    assert_eq!(
        fixture.state.store.list_node_actions(&node_id).unwrap()[0].kind,
        "ensure"
    );

    drop(lease);
    let actions = fixture.state.store.list_node_actions(&node_id).unwrap();
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[1].kind, "release");
}

#[tokio::test]
async fn heartbeat_over_another_interface_does_not_allow_immediate_relay() {
    let fixture = Fixture::new(1);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "never", true);
    let node_id = profile.node_id.as_deref().unwrap();
    fixture
        .state
        .store
        .heartbeat_node(
            node_id,
            "linux",
            &json!({
        "network":{"mode":"interface","interface":"wired","status":"disconnected","addresses":[]}
            }),
        )
        .unwrap();
    let mut lease = RuntimeLease::new(fixture.state.clone(), profile.clone(), "wired-only".into());
    let error = prepare_profile(&fixture.state, &mut lease)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Selected network interface 'wired'")
    );
    assert!(
        fixture
            .state
            .store
            .list_node_actions(node_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn on_demand_online_profile_still_checks_readiness() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let probed = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&probed);
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move || {
            let handler_flag = Arc::clone(&handler_flag);
            async move {
                handler_flag.store(true, Ordering::SeqCst);
                axum::Json(serde_json::json!({"data":[{"id":"coding-model"}]}))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let fixture = Fixture::new(2);
    let profile = fixture.profile(&format!("http://{address}/v1"), "on-demand", true);
    let mut lease = RuntimeLease::new(fixture.state.clone(), profile.clone(), "checked".into());

    let node_store = fixture.state.store.clone();
    let node_id = profile.node_id.clone().unwrap();
    let node = tokio::spawn(async move {
        loop {
            if let Some(action) = node_store.poll_node_action(&node_id).unwrap() {
                node_store
                    .complete_node_action(&node_id, &action.id, true, json!({"ready":true}))
                    .unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    prepare_profile(&fixture.state, &mut lease).await.unwrap();
    node.await.unwrap();
    assert!(probed.load(Ordering::SeqCst));
    server.abort();
}

#[tokio::test]
async fn managed_ollama_preparation_uses_the_nodes_context_model() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!({
                "data":[{"id":"poundlock-context-fixture:latest"}]
            }))
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let fixture = Fixture::new(2);
    let mut profile = fixture.profile(&format!("http://{address}/v1"), "on-demand", true);
    profile.runtime_profile = Some("ollama".into());
    let mut lease = RuntimeLease::new(fixture.state.clone(), profile.clone(), "checked".into());

    let node_store = fixture.state.store.clone();
    let node_id = profile.node_id.clone().unwrap();
    let node = tokio::spawn(async move {
        loop {
            if let Some(action) = node_store.poll_node_action(&node_id).unwrap() {
                node_store
                    .complete_node_action(
                        &node_id,
                        &action.id,
                        true,
                        json!({"resolved_model_id":"poundlock-context-fixture:latest"}),
                    )
                    .unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let resolved = prepare_profile(&fixture.state, &mut lease).await.unwrap();
    assert_eq!(resolved, "poundlock-context-fixture:latest");
    node.await.unwrap();
    server.abort();
}

#[tokio::test]
async fn manual_profile_fails_fast_when_its_node_is_offline() {
    let fixture = Fixture::with_offline_threshold(60, -1);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "manual", true);
    let mut lease = RuntimeLease::new(fixture.state.clone(), profile.clone(), "manual".into());

    let result = tokio::time::timeout(
        Duration::from_millis(100),
        prepare_profile(&fixture.state, &mut lease),
    )
    .await
    .expect("manual readiness must not wait for an offline node");
    assert!(result.is_err());
}

#[tokio::test]
async fn cancellation_after_ensure_enqueues_release() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = Fixture::new(60);
    let profile = fixture.profile(&format!("http://{address}/v1"), "on-demand", true);
    let node_id = profile.node_id.clone().unwrap();
    let state = fixture.state.clone();
    let handle = tokio::spawn(async move {
        let mut lease = RuntimeLease::new(state.clone(), profile.clone(), "cancelled".into());
        let _ = prepare_profile(&state, &mut lease).await;
    });
    let accept = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fixture
                .state
                .store
                .list_node_actions(&node_id)
                .unwrap()
                .iter()
                .any(|action| action.kind == "ensure")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    handle.abort();
    let _ = handle.await;
    accept.abort();
    let actions = fixture.state.store.list_node_actions(&node_id).unwrap();
    assert!(actions.iter().any(|action| action.kind == "release"));
}

#[tokio::test]
async fn advertised_model_cannot_bypass_failed_node_preparation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::Json(json!({"data":[{"id":"coding-model"}]}))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let fixture = Fixture::new(3);
    let profile = fixture.profile(&format!("http://{address}/v1"), "on-demand", true);
    let node_id = profile.node_id.clone().unwrap();
    let node_store = fixture.state.store.clone();
    let node = tokio::spawn(async move {
        loop {
            if let Some(action) = node_store.poll_node_action(&node_id).unwrap() {
                node_store
                    .complete_node_action(
                        &node_id,
                        &action.id,
                        false,
                        json!({"message":"model could not start"}),
                    )
                    .unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let mut lease = RuntimeLease::new(fixture.state.clone(), profile, "failed-start".into());
    let failure = prepare_profile(&fixture.state, &mut lease)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("runtime ensure failed"));
    // Typed, so the relay can tell a node that answered and failed — retry
    // starts clean — from a machine that is offline or paused.
    assert!(failure.downcast_ref::<EnsureFailed>().is_some());
    node.await.unwrap();
    server.abort();
}

#[tokio::test]
async fn offline_node_receives_magic_packet_before_runtime_preparation() {
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let broadcast = udp.local_addr().unwrap().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::Json(json!({"data":[{"id":"coding-model"}]}))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let fixture = Fixture::new(8);
    let profile = fixture.profile(&format!("http://{address}/v1"), "on-demand", true);
    let node_id = profile.node_id.clone().unwrap();
    fixture
        .state
        .store
        .configure_wol(&node_id, Some("02:11:22:33:44:55"), Some(&broadcast))
        .unwrap();
    rusqlite::Connection::open(fixture._directory.path().join("poundlock.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE nodes SET last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&node_id],
        )
        .unwrap();
    assert_eq!(
        fixture
            .state
            .store
            .node(&node_id, 15)
            .unwrap()
            .unwrap()
            .status,
        "offline"
    );
    let node_store = fixture.state.store.clone();
    let observed_id = node_id.clone();
    let node = tokio::spawn(async move {
        let mut packet = [0_u8; 102];
        let length = udp.recv(&mut packet).await.unwrap();
        assert_eq!(length, 102);
        assert_eq!(&packet[..6], &[255; 6]);
        for mac in packet[6..].as_chunks::<6>().0 {
            assert_eq!(mac, &[2, 17, 34, 51, 68, 85]);
        }
        assert!(
            node_store
                .list_node_actions(&observed_id)
                .unwrap()
                .is_empty()
        );
        node_store
            .heartbeat_node(&observed_id, "linux", &json!({"runtimes":["coding"]}))
            .unwrap();
        loop {
            if let Some(action) = node_store.poll_node_action(&observed_id).unwrap() {
                assert_eq!(action.kind, "ensure");
                node_store
                    .complete_node_action(&observed_id, &action.id, true, json!({"ready":true}))
                    .unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let mut lease = RuntimeLease::new(fixture.state.clone(), profile, "wake-flow".into());
    prepare_profile(&fixture.state, &mut lease).await.unwrap();
    node.await.unwrap();
    drop(lease);
    let actions = fixture.state.store.list_node_actions(&node_id).unwrap();
    assert_eq!(
        actions
            .iter()
            .map(|action| action.kind.as_str())
            .collect::<Vec<_>>(),
        ["ensure", "release"]
    );
    server.abort();
}

#[tokio::test]
async fn node_sleeping_after_a_fresh_heartbeat_is_woken_without_duplicate_ensure() {
    let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::Json(json!({"data":[{"id":"coding-model"}]}))
                }),
            ),
        )
        .await
        .unwrap();
    });
    let fixture = Fixture::new(8);
    let profile = fixture.profile(&format!("http://{address}/v1"), "on-demand", true);
    let node_id = profile.node_id.clone().unwrap();
    fixture
        .state
        .store
        .configure_wol(
            &node_id,
            Some("02:11:22:33:44:55"),
            Some(&udp.local_addr().unwrap().to_string()),
        )
        .unwrap();
    let state = fixture.state.clone();
    let preparation = tokio::spawn(async move {
        let mut lease = RuntimeLease::new(state.clone(), profile, "sleep-race".into());
        prepare_profile(&state, &mut lease).await
    });
    let ensure = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(action) = fixture
                .state
                .store
                .list_node_actions(&node_id)
                .unwrap()
                .into_iter()
                .find(|a| a.kind == "ensure")
            {
                break action;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    rusqlite::Connection::open(fixture._directory.path().join("poundlock.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE nodes SET last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&node_id],
        )
        .unwrap();
    let mut packet = [0_u8; 102];
    let received = tokio::time::timeout(Duration::from_secs(3), udp.recv(&mut packet)).await;
    if received.is_err() {
        preparation.abort();
        server.abort();
        panic!("No wake packet after the initially-online node went offline");
    }
    assert_eq!(received.unwrap().unwrap(), 102);
    assert_eq!(&packet[..6], &[255; 6]);
    fixture
        .state
        .store
        .heartbeat_node(&node_id, "linux", &json!({"runtimes":["coding"]}))
        .unwrap();
    let polled = fixture
        .state
        .store
        .poll_node_action(&node_id)
        .unwrap()
        .unwrap();
    assert_eq!(polled.id, ensure.id);
    fixture
        .state
        .store
        .complete_node_action(&node_id, &polled.id, true, json!({"ready":true}))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), preparation)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let actions = fixture.state.store.list_node_actions(&node_id).unwrap();
    assert_eq!(actions.iter().filter(|a| a.kind == "ensure").count(), 1);
    server.abort();
}

#[tokio::test]
async fn cancellation_before_ensure_fences_queued_renewal() {
    let fixture = Fixture::new(60);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "on-demand", true);
    let node_id = profile.node_id.clone().unwrap();
    let lease = RuntimeLease::new(
        fixture.state.clone(),
        profile,
        "cancel-before-ensure".into(),
    );
    let renewal = fixture
        .state
        .store
        .enqueue_action(
            &node_id,
            None,
            "renew",
            json!({"lease_id":"cancel-before-ensure"}),
        )
        .unwrap();
    drop(lease);
    assert_eq!(
        fixture
            .state
            .store
            .action(&renewal.id)
            .unwrap()
            .unwrap()
            .status,
        "failed"
    );
    assert_eq!(
        fixture
            .state
            .store
            .poll_node_action(&node_id)
            .unwrap()
            .unwrap()
            .kind,
        "release"
    );
}

#[test]
fn runtime_availability_distinguishes_node_states_without_side_effects() {
    let online = Fixture::new(1);
    assert_eq!(
        runtime_availability(
            &online.state,
            &online.profile("http://127.0.0.1:9/v1", "manual", false)
        )
        .unwrap(),
        None,
        "profiles without a node have no runtime admission state"
    );
    let manual = online.profile("http://127.0.0.1:9/v1", "manual", true);
    assert_eq!(runtime_availability(&online.state, &manual).unwrap(), None);
    online
        .state
        .store
        .set_node_maintenance(manual.node_id.as_deref().unwrap(), None, true)
        .unwrap();
    assert_eq!(
        runtime_availability(&online.state, &manual).unwrap(),
        Some(("blocked", "runtime_node_unavailable"))
    );

    let offline = Fixture::with_offline_threshold(1, -1);
    let manual = offline.profile("http://127.0.0.1:9/v1", "manual", true);
    assert_eq!(
        runtime_availability(&offline.state, &manual).unwrap(),
        Some(("blocked", "runtime_node_offline"))
    );
    let never = offline.profile("http://127.0.0.1:9/v1", "never", true);
    assert_eq!(runtime_availability(&offline.state, &never).unwrap(), None);
    let on_demand = offline.profile("http://127.0.0.1:9/v1", "on-demand", true);
    assert_eq!(
        runtime_availability(&offline.state, &on_demand).unwrap(),
        Some(("blocked", "runtime_wake_unavailable"))
    );
    offline
        .state
        .store
        .configure_wol(
            on_demand.node_id.as_deref().unwrap(),
            Some("02:11:22:33:44:55"),
            Some("127.0.0.1:9"),
        )
        .unwrap();
    assert_eq!(
        runtime_availability(&offline.state, &on_demand).unwrap(),
        Some(("preparation_required", "runtime_wake_required"))
    );
    assert!(
        offline
            .state
            .store
            .list_node_actions(on_demand.node_id.as_deref().unwrap())
            .unwrap()
            .is_empty(),
        "availability checks must not enqueue runtime actions"
    );
}

/// What the node's poll loop does once it holds the action: run it, then
/// report the outcome `delay` later.
fn node_settles_action_after(
    store: Store,
    node_id: String,
    delay: Duration,
    success: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let action = store.poll_node_action(&node_id).unwrap().unwrap();
        store
            .complete_node_action(&node_id, &action.id, success, json!({"ready":success}))
            .unwrap();
    })
}

#[tokio::test(start_paused = true)]
async fn waiting_for_an_action_wakes_when_the_node_completes_it() {
    let fixture = Fixture::new(60);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "on-demand", true);
    let action = enqueue_ensure(&fixture.state, &profile, "wake").unwrap();
    let node = node_settles_action_after(
        fixture.state.store.clone(),
        profile.node_id.clone().unwrap(),
        Duration::from_millis(40),
        true,
    );
    let started = tokio::time::Instant::now();
    let result = wait_for_action(
        &fixture.state,
        &profile,
        &action,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(result["ready"], true);
    // Not the one-second recheck: the completion itself woke the waiter.
    assert_eq!(started.elapsed(), Duration::from_millis(40));
    node.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn waiting_for_an_action_wakes_when_the_node_fails_it() {
    let fixture = Fixture::new(60);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "on-demand", true);
    let action = enqueue_ensure(&fixture.state, &profile, "wake").unwrap();
    let node = node_settles_action_after(
        fixture.state.store.clone(),
        profile.node_id.clone().unwrap(),
        Duration::from_millis(40),
        false,
    );
    let started = tokio::time::Instant::now();
    let error = wait_for_action(
        &fixture.state,
        &profile,
        &action,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .unwrap_err();
    assert!(error.downcast_ref::<EnsureFailed>().is_some(), "{error:#}");
    assert_eq!(started.elapsed(), Duration::from_millis(40));
    node.await.unwrap();
}

/// A change made without announcing it is still found, at the old one-second
/// cadence.
#[tokio::test(start_paused = true)]
async fn waiting_for_an_action_still_rechecks_when_nothing_announces_a_change() {
    let fixture = Fixture::new(60);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "on-demand", true);
    let action = enqueue_ensure(&fixture.state, &profile, "quiet").unwrap();
    let database = fixture._directory.path().join("poundlock.sqlite3");
    let action_id = action.id.clone();
    let quiet = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        rusqlite::Connection::open(database)
            .unwrap()
            .execute(
                "UPDATE node_actions SET status='completed',result_json='{}' WHERE id=?1",
                [&action_id],
            )
            .unwrap();
    });
    let started = tokio::time::Instant::now();
    wait_for_action(
        &fixture.state,
        &profile,
        &action,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    quiet.await.unwrap();
}

#[tokio::test]
async fn waiting_for_an_action_gives_up_at_its_deadline() {
    let fixture = Fixture::new(60);
    let profile = fixture.profile("http://127.0.0.1:9/v1", "on-demand", true);
    let action = enqueue_ensure(&fixture.state, &profile, "stuck").unwrap();
    let error = wait_for_action(
        &fixture.state,
        &profile,
        &action,
        Instant::now() + Duration::from_millis(200),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("runtime ensure did not complete before the deadline"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<EnsureFailed>().is_none());
}

/// "When a request arrives", as a client sees it: "Start the model, don't
/// wake the machine" (`manual`) refuses at once while the machine sleeps,
/// and "I keep it running" (`never`) forwards and then says plainly why the
/// server did not answer.
#[tokio::test]
async fn readiness_failures_say_the_machine_is_asleep_or_the_server_is_down() {
    use crate::server::ops_tests::send;
    let fixture = Fixture::with_offline_threshold(60, -1);
    let store = &fixture.state.store;
    let endpoint = store
        .create_endpoint(&CreateEndpoint {
            name: "unreachable".into(),
            kind: "openai".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: String::new(),
            managed_identity: Default::default(),
        })
        .unwrap();
    let node_id = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "sleeper".into(),
            platform: "linux".into(),
            capabilities: serde_json::json!({"runtimes":["coding"]}),
        })
        .unwrap()
        .node_id;
    for (name, wake_policy, node) in [
        ("start-no-wake", "manual", Some(&node_id)),
        ("kept-running", "never", Some(&node_id)),
        ("always-on", "never", None),
    ] {
        store
            .create_model_profile(
                &serde_json::from_value(serde_json::json!({
                    "name": name, "endpoint_id": endpoint.id, "model_id": "coding-model",
                    "context_tokens": 8192, "max_output_tokens": 1024, "uses": ["interactive"],
                    "wake_policy": wake_policy, "data_destination": "operator_network",
                    "node_id": node, "runtime_profile": node.map(|_| "coding"),
                }))
                .unwrap(),
            )
            .unwrap();
    }
    let token = store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "client".into(),
            permissions: vec!["inference:invoke".into()],
        })
        .unwrap()
        .token;
    let app = crate::server::router(fixture.state.clone());
    let ask = |model: &str| serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});

    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &token,
        ask("start-no-wake"),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "{body}"
    );
    assert_eq!(body["error"]["code"], "runtime_node_offline");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("asleep"),
        "{body}"
    );

    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &token,
        ask("kept-running"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"]["code"], "upstream_connection_failed");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("asleep"),
        "{body}"
    );

    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        ask("always-on"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not running"),
        "{body}"
    );
}
