//! What a request to a node-managed profile pays when the model server is
//! already up: the real controller, the real node poll loop over a runtime
//! that answers at once, and a stand-in upstream. Nothing here is slow except
//! the controller and node waiting on each other.
use super::ops_tests::state_with_artifacts;
use super::*;
use crate::domain::{CreateApiToken, EnrollNode, EnrolledNode};
use axum::extract::State;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Which controller the node talks to: the one under test, or the same one
/// with the `wait_seconds` request option stripped, which is what a controller
/// that predates it does with a poll.
#[derive(Clone, Copy, PartialEq)]
enum Controller {
    Current,
    IgnoresWaitSeconds,
}

/// Which node polls: the poll loop under test, or the loop this release
/// replaces, reproduced request for request.
#[derive(Clone, Copy, PartialEq)]
enum Node {
    Current,
    Older,
}

/// Counts the polls that reach the controller, and optionally drops the
/// option a controller without long-poll support would not know.
struct Tap {
    polls: AtomicUsize,
    ignore_wait_seconds: bool,
}

async fn tap(
    State(tap): State<Arc<Tap>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut request = request;
    if request.uri().path().ends_with("/actions/poll") {
        tap.polls.fetch_add(1, Ordering::SeqCst);
        if tap.ignore_wait_seconds {
            let (mut parts, _) = request.into_parts();
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            request = axum::extract::Request::from_parts(parts, axum::body::Body::from("{}"));
        }
    }
    next.run(request).await
}

/// The node loop before long-poll: an immediate poll, then a sleep of the
/// interval whatever happened.
async fn older_node(controller: String, node: EnrolledNode, interval: Duration) {
    let http = reqwest::Client::new();
    let node_url = format!("{controller}/node/v1/nodes/{}", node.node_id);
    loop {
        let action: Option<Value> = http
            .post(format!("{node_url}/actions/poll"))
            .bearer_auth(&node.credential)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(action) = action {
            http.post(format!(
                "{node_url}/actions/{}/complete",
                action["id"].as_str().unwrap()
            ))
            .bearer_auth(&node.credential)
            .json(&json!({"success":true,"result":{"adapter":"noop","status":"ready"}}))
            .send()
            .await
            .unwrap();
        }
        tokio::time::sleep(interval).await;
    }
}

/// A controller listening on a real port, a node-managed profile served by an
/// upstream that is already up, and a node polling it.
struct Rig {
    _directory: tempfile::TempDir,
    controller: String,
    upstream: String,
    token: String,
    http: reqwest::Client,
    tap: Arc<Tap>,
    upstream_connections: Arc<AtomicUsize>,
    node: tokio::task::JoinHandle<()>,
}

impl Rig {
    async fn start(controller: Controller, node: Node, heartbeat: Duration) -> Self {
        let upstream_connections = Arc::new(AtomicUsize::new(0));
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = {
            use axum::serve::ListenerExt;
            let connections = upstream_connections.clone();
            upstream.tap_io(move |_| {
                connections.fetch_add(1, Ordering::SeqCst);
            })
        };
        let upstream_root = format!(
            "http://{}",
            axum::serve::Listener::local_addr(&upstream).unwrap()
        );
        let upstream_url = format!("{upstream_root}/v1");
        tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/v1/models",
                    axum::routing::get(|| async {
                        axum::Json(json!({"data":[{"id":"upstream"}]}))
                    }),
                )
                .route(
                    "/v1/chat/completions",
                    axum::routing::post(|| async {
                        axum::Json(
                            json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}),
                        )
                    }),
                );
            axum::serve(upstream, app).await.unwrap();
        });

        let (directory, mut state) = state_with_artifacts(None);
        // The node's heartbeat is a separate task this rig does not run.
        state.config = Arc::new(AppConfig {
            node_offline_seconds: 3600,
            ..(*state.config).clone()
        });
        // A controller with a small fleet: node lookups read every machine's
        // inventory unless they ask for just one.
        for extra in 0..4 {
            state
                .store
                .enroll_node(&EnrollNode {
                    token: state.store.create_enrollment_token(15, None).unwrap(),
                    name: format!("other-{extra}"),
                    platform: "linux".into(),
                    capabilities: json!({"inventory":{"software":
                        (0..250).map(|i| format!("package-{i}-with-a-descriptive-name")).collect::<Vec<_>>()
                    }}),
                })
                .unwrap();
        }
        // The disks this runs on may not wait for a flush the way a server's
        // do. Setting POUNDLOCK_BENCH_FLUSH makes SQLite ask macOS for a real
        // one, which is what every commit costs on a Linux host.
        if std::env::var_os("POUNDLOCK_BENCH_FLUSH").is_some() {
            state
                .store
                .connection()
                .unwrap()
                .execute_batch("PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;")
                .unwrap();
        }
        let endpoint = state
            .store
            .create_endpoint(
                &serde_json::from_value(json!({
                    "name":"up","kind":"openai","base_url":upstream_url,"api_key":""
                }))
                .unwrap(),
            )
            .unwrap();
        let enrolled = state
            .store
            .enroll_node(&EnrollNode {
                token: state.store.create_enrollment_token(15, None).unwrap(),
                name: "gpu".into(),
                platform: "linux".into(),
                capabilities: json!({}),
            })
            .unwrap();
        state
            .store
            .create_model_profile(
                &serde_json::from_value(json!({
                    "name":"coding",
                    "endpoint_id":endpoint.id,
                    "model_id":"upstream",
                    "context_tokens":8192,
                    "max_output_tokens":1024,
                    "workload_class":"interactive-coding",
                    "wake_policy":"on-demand",
                    "data_destination":"operator_network",
                    "node_id":enrolled.node_id,
                    "runtime_profile":"coding"
                }))
                .unwrap(),
            )
            .unwrap();
        let token = state
            .store
            .create_api_token(&CreateApiToken {
                name: "client".into(),
                permissions: vec!["models:read".into(), "inference:invoke".into()],
            })
            .unwrap()
            .token;

        let tap_state = Arc::new(Tap {
            polls: AtomicUsize::new(0),
            ignore_wait_seconds: controller == Controller::IgnoresWaitSeconds,
        });
        let app = router(state).layer(axum::middleware::from_fn_with_state(tap_state.clone(), tap));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(crate::tcp::nodelay(listener), app)
                .await
                .unwrap()
        });
        let node = tokio::spawn({
            let url = url.clone();
            async move {
                match node {
                    Node::Current => {
                        crate::node::testing::serve_always_ready(
                            &url,
                            &enrolled.node_id,
                            &enrolled.credential,
                            heartbeat,
                        )
                        .await
                    }
                    Node::Older => older_node(url, enrolled, heartbeat).await,
                }
            }
        });
        Self {
            _directory: directory,
            controller: url,
            upstream: upstream_root,
            token,
            http: reqwest::Client::new(),
            tap: tap_state,
            upstream_connections,
            node,
        }
    }

    /// One one-token chat completion through `/v1/chat/completions`, and how
    /// long it took.
    async fn chat(&self) -> Duration {
        self.completion(&self.controller, Some(&self.token)).await
    }

    /// The same request straight to the upstream model server.
    async fn chat_direct(&self) -> Duration {
        self.completion(&self.upstream, None).await
    }

    async fn completion(&self, root: &str, token: Option<&str>) -> Duration {
        let started = Instant::now();
        let mut request = self
            .http
            .post(format!("{root}/v1/chat/completions"))
            .json(&json!({
                "model":"coding","max_tokens":1,
                "messages":[{"role":"user","content":"hi"}]
            }));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        started.elapsed()
    }

    /// Sequential requests, each starting as soon as the last one answered.
    async fn chat_sequentially(&self, requests: usize) -> Vec<Duration> {
        let mut latencies = Vec::new();
        for _ in 0..requests {
            latencies.push(self.chat().await);
        }
        latencies
    }

    fn upstream_connections(&self) -> usize {
        self.upstream_connections.load(Ordering::SeqCst)
    }

    fn polls(&self) -> usize {
        self.tap.polls.load(Ordering::SeqCst)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.node.abort();
    }
}

fn percentile(latencies: &[Duration], fraction: f64) -> Duration {
    let mut sorted = latencies.to_vec();
    sorted.sort();
    sorted[((sorted.len() - 1) as f64 * fraction).round() as usize]
}

#[tokio::test]
async fn warm_requests_wait_only_for_the_work_itself() {
    let rig = Rig::start(Controller::Current, Node::Current, Duration::from_secs(3)).await;
    let latencies = rig.chat_sequentially(12).await;
    eprintln!(
        "warm path, current node and controller: p50 {:?}, p95 {:?}, all {latencies:?}",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.95)
    );
    // The node's poll interval is three seconds; none of it is on the path.
    assert!(
        percentile(&latencies, 0.5) < Duration::from_millis(500),
        "{latencies:?}"
    );
    assert!(
        *latencies.iter().max().unwrap() < Duration::from_secs(2),
        "{latencies:?}"
    );
}

/// Node upgraded before its controller: the option is ignored, requests still
/// complete, and the node polls at its interval rather than in a loop.
#[tokio::test]
async fn a_node_against_a_controller_without_long_poll_keeps_its_interval() {
    let rig = Rig::start(
        Controller::IgnoresWaitSeconds,
        Node::Current,
        Duration::from_secs(1),
    )
    .await;
    let started = Instant::now();
    let latencies = rig.chat_sequentially(2).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    // Each of the two requests waits for a poll or two, as before; nothing
    // here may exceed a handful of intervals.
    assert!(
        *latencies.iter().max().unwrap() < Duration::from_secs(6),
        "{latencies:?}"
    );
    let seconds = started.elapsed().as_secs() as usize;
    assert!(
        rig.polls() <= 2 * (seconds + 2),
        "{} polls in {seconds}s",
        rig.polls()
    );
}

/// Controller upgraded before its nodes: an agent that sends `{}` is served
/// as it always was.
#[tokio::test]
async fn an_older_node_still_serves_requests_from_the_current_controller() {
    let rig = Rig::start(Controller::Current, Node::Older, Duration::from_secs(1)).await;
    let latencies = rig.chat_sequentially(2).await;
    assert!(
        *latencies.iter().max().unwrap() < Duration::from_secs(6),
        "{latencies:?}"
    );
}

/// What the controller adds to a request, over calling the model server
/// directly: the same request both ways, alternating so drift in the machine's
/// load lands on both. Prints the distributions; run with `--nocapture`. It is
/// a multi-threaded runtime because the controller is one: a commit that waits
/// on the disk must not stall the client and the stand-in upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_controller_adds_little_to_a_warm_request() {
    let rig = Rig::start(Controller::Current, Node::Current, Duration::from_secs(3)).await;
    for _ in 0..10 {
        rig.chat().await;
        rig.chat_direct().await;
    }
    let (mut through, mut direct) = (Vec::new(), Vec::new());
    for _ in 0..200 {
        through.push(rig.chat().await);
        direct.push(rig.chat_direct().await);
    }
    let added =
        |fraction| percentile(&through, fraction).saturating_sub(percentile(&direct, fraction));
    eprintln!(
        "warm path over 200 requests: direct p50 {:?} p95 {:?}; controller p50 {:?} p95 {:?}; added p50 {:?} p95 {:?}",
        percentile(&direct, 0.5),
        percentile(&direct, 0.95),
        percentile(&through, 0.5),
        percentile(&through, 0.95),
        added(0.5),
        added(0.95),
    );
    // Milliseconds today. The bound is loose enough for a slow disk and tight
    // enough to catch a poll interval or a sleep coming back onto the path.
    assert!(added(0.5) < Duration::from_millis(100), "{:?}", added(0.5));
}

/// A new connection per request would add a handshake, and on a remote model
/// server a slow start, to every request. The relay's client and the readiness
/// probe's client each keep theirs.
#[tokio::test]
async fn the_relay_reuses_its_connections_to_the_endpoint() {
    let rig = Rig::start(Controller::Current, Node::Current, Duration::from_secs(3)).await;
    rig.chat_sequentially(20).await;
    assert!(
        rig.upstream_connections() <= 2,
        "{} connections for 20 requests",
        rig.upstream_connections()
    );
}
