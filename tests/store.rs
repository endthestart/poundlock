use poundlock::crypto::SecretBox;
use poundlock::domain::*;
use poundlock::store::Store;
use serde_json::json;

fn store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    (directory, store)
}

fn configured(store: &Store) -> (Endpoint, ModelProfile, Project) {
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
            name: "coding".into(),
            aliases: Vec::new(),
            endpoint_id: endpoint.id.clone(),
            model_id: "model".into(),
            model_label: None,
            context_tokens: 8192,
            max_output_tokens: 4096,
            uses: None,
            workload_class: Some("durable-coding-agent".into()),
            wake_policy: "never".into(),
            idle_seconds: None,
            data_destination: "unknown".into(),
            thinking: "model_default".into(),
            thinking_budget_tokens: None,
            capabilities: Vec::new(),
            node_id: None,
            runtime_profile: None,
        })
        .unwrap();
    let project = store
        .create_project(&CreateProject {
            name: "product".into(),
            roots: vec!["repo".into()],
            instruction_files: Vec::new(),
        })
        .unwrap();
    (endpoint, model, project)
}

#[test]
fn secrets_round_trip_without_public_exposure() {
    let (_directory, store) = store();
    let (endpoint, _, _) = configured(&store);
    assert!(store.list_endpoints().unwrap()[0].api_key_set);
    assert_eq!(
        store
            .endpoint_secret(&endpoint.id)
            .unwrap()
            .unwrap()
            .api_key,
        "private-key"
    );
    assert!(
        !serde_json::to_string(&store.list_endpoints().unwrap())
            .unwrap()
            .contains("private-key")
    );
}

fn endpoint_of(store: &Store, name: &str, kind: &str) -> Endpoint {
    store
        .create_endpoint(&CreateEndpoint {
            name: name.into(),
            kind: kind.into(),
            base_url: format!("http://{name}.invalid/v1"),
            api_key: String::new(),
            managed_identity: Default::default(),
        })
        .unwrap()
}

fn model_on(store: &Store, name: &str, endpoint: &Endpoint, destination: &str) -> ModelProfile {
    store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name": name, "endpoint_id": endpoint.id, "model_id": name,
                "context_tokens": 8192, "max_output_tokens": 1024, "uses": ["durable"],
                "wake_policy": "never", "data_destination": destination
            }))
            .unwrap(),
        )
        .unwrap()
}

fn reopen(directory: &tempfile::TempDir) -> Store {
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap()
}

/// A database from before servers declared their destination: every model
/// carried its own, and nothing recorded a server's.
fn as_before_server_destinations(directory: &tempfile::TempDir, destinations: &[(&str, &str)]) {
    let db = rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap();
    db.execute_batch(
        "ALTER TABLE endpoints DROP COLUMN data_destination;
         ALTER TABLE model_profiles DROP COLUMN destination_from_endpoint;",
    )
    .unwrap();
    for (name, destination) in destinations {
        db.execute(
            "UPDATE model_profiles SET data_destination=?2 WHERE name=?1",
            [name, destination],
        )
        .unwrap();
    }
}

/// What the routing revision depends on, read straight from the database:
/// if none of it moves, no client is asked to approve a route again.
fn routes(directory: &tempfile::TempDir) -> Vec<(String, String, String, String)> {
    let db = rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap();
    let mut statement = db
        .prepare(
            "SELECT m.name,m.data_destination,m.revision_nonce,e.revision_nonce
             FROM model_profiles m JOIN endpoints e ON e.id=m.endpoint_id ORDER BY m.name",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn servers_take_over_destinations_without_changing_any_model() {
    let (directory, store) = store();
    let agreeing = endpoint_of(&store, "agreeing", "openai");
    let mixed = endpoint_of(&store, "mixed", "openai");
    let undeclared = endpoint_of(&store, "undeclared", "openai");
    let empty = endpoint_of(&store, "empty", "openai");
    let gateway = endpoint_of(&store, "gateway", "litellm");
    for (name, endpoint) in [
        ("agreeing-a", &agreeing),
        ("agreeing-b", &agreeing),
        ("agreeing-removed", &agreeing),
        ("mixed-local", &mixed),
        ("mixed-external", &mixed),
        ("undeclared-only", &undeclared),
    ] {
        model_on(&store, name, endpoint, "unknown");
    }
    model_on(&store, "route-external", &gateway, "external");
    model_on(&store, "route-local", &gateway, "operator_network");
    let removed = store
        .model_profile_by_name("agreeing-removed")
        .unwrap()
        .unwrap();
    store.remove_model_profile(&removed.id).unwrap();
    drop(store);
    as_before_server_destinations(
        &directory,
        &[
            ("agreeing-a", "operator_network"),
            ("agreeing-b", "operator_network"),
            // A removed model keeps its row for accepted jobs and must not
            // count towards its old server's declaration.
            (&format!("removed-{}", removed.id), "external"),
            ("mixed-local", "operator_network"),
            ("mixed-external", "external"),
        ],
    );
    let before = routes(&directory);

    let store = reopen(&directory);
    assert_eq!(
        routes(&directory),
        before,
        "no destination or revision moved"
    );
    let model = |name: &str| store.model_profile_by_name(name).unwrap().unwrap();
    let server = |endpoint: &Endpoint| {
        store
            .list_endpoints()
            .unwrap()
            .into_iter()
            .find(|item| item.id == endpoint.id)
            .unwrap()
            .data_destination
    };
    // Its models agreed, so the server declares it and they inherit it.
    assert_eq!(server(&agreeing).as_deref(), Some("operator_network"));
    for name in ["agreeing-a", "agreeing-b"] {
        assert_eq!(model(name).data_destination, "operator_network");
        assert_eq!(model(name).data_destination_source, "endpoint");
    }
    // They disagreed: nothing is chosen for the operator, and each keeps its own.
    assert_eq!(server(&mixed).as_deref(), Some("unknown"));
    assert_eq!(model("mixed-local").data_destination, "operator_network");
    assert_eq!(model("mixed-external").data_destination, "external");
    assert_eq!(model("mixed-local").data_destination_source, "profile");
    // Unknown stays unknown; a server without models declares nothing.
    assert_eq!(server(&undeclared).as_deref(), Some("unknown"));
    assert_eq!(model("undeclared-only").data_destination, "unknown");
    assert_eq!(model("undeclared-only").data_destination_source, "endpoint");
    assert_eq!(server(&empty).as_deref(), Some("unknown"));
    // Gateway routes keep one declaration each.
    assert_eq!(server(&gateway), None);
    assert_eq!(model("route-external").data_destination, "external");
    assert_eq!(model("route-local").data_destination, "operator_network");
    assert_eq!(model("route-local").data_destination_source, "profile");
    // Opening again changes nothing.
    drop(store);
    let store = reopen(&directory);
    assert_eq!(routes(&directory), before);

    // Declaring the mixed server applies to every model on it, and moves the
    // routing revision only of the model whose destination changed.
    let local_revision = store
        .routing_revision(&store.model_profile_by_name("mixed-local").unwrap().unwrap())
        .unwrap();
    let external_revision = store
        .routing_revision(
            &store
                .model_profile_by_name("mixed-external")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
    assert!(
        store
            .set_endpoint_destination(&mixed.id, "external")
            .unwrap()
    );
    let local = store.model_profile_by_name("mixed-local").unwrap().unwrap();
    let external = store
        .model_profile_by_name("mixed-external")
        .unwrap()
        .unwrap();
    assert_eq!(local.data_destination, "external");
    assert_eq!(local.data_destination_source, "endpoint");
    assert_ne!(store.routing_revision(&local).unwrap(), local_revision);
    assert_eq!(
        store.routing_revision(&external).unwrap(),
        external_revision
    );
    // A gateway has no server-wide declaration to set.
    assert!(
        store
            .set_endpoint_destination(&gateway.id, "external")
            .is_err()
    );
    assert!(
        store
            .set_endpoint_destination(&mixed.id, "guessed")
            .is_err()
    );
}

#[test]
fn models_on_a_server_inherit_its_destination_and_cannot_contradict_it() {
    let (_directory, store) = store();
    let server = endpoint_of(&store, "server", "openai");
    // A server nobody declared takes its first model's declaration.
    let first = model_on(&store, "first", &server, "operator_network");
    assert_eq!(first.data_destination, "operator_network");
    assert_eq!(first.data_destination_source, "endpoint");
    assert_eq!(
        store.list_endpoints().unwrap()[0]
            .data_destination
            .as_deref(),
        Some("operator_network")
    );
    // Later models inherit it, may restate it, and may not contradict it.
    assert_eq!(
        model_on(&store, "second", &server, "unknown").data_destination,
        "operator_network"
    );
    assert_eq!(
        model_on(&store, "third", &server, "operator_network").data_destination,
        "operator_network"
    );
    let contradicting: CreateModelProfile = serde_json::from_value(json!({
        "name": "fourth", "endpoint_id": server.id, "model_id": "x", "context_tokens": 8192,
        "max_output_tokens": 1024, "uses": ["durable"], "wake_policy": "never",
        "data_destination": "external"
    }))
    .unwrap();
    let refused = store.create_model_profile(&contradicting).unwrap_err();
    assert!(
        refused.to_string().contains("change it on the server"),
        "{refused}"
    );
    let patch = |profile: &ModelProfile, body: serde_json::Value| {
        store.patch_model_profile(&profile.id, &serde_json::from_value(body).unwrap())
    };
    assert!(patch(&first, json!({"data_destination": "external"})).is_err());
    // Changing the model on the same server keeps the server's declaration.
    let moved = patch(&first, json!({"model_id": "another"}))
        .unwrap()
        .unwrap();
    assert_eq!(moved.data_destination, "operator_network");
    // Changing the server's declaration changes every model on it.
    store
        .set_endpoint_destination(&server.id, "external")
        .unwrap();
    assert!(
        store
            .list_model_profiles()
            .unwrap()
            .iter()
            .all(|profile| profile.data_destination == "external")
    );
    // Moving to a server that declares otherwise needs the new destination stated.
    let other = endpoint_of(&store, "other", "openai");
    store
        .set_endpoint_destination(&other.id, "operator_network")
        .unwrap();
    let refused = patch(&first, json!({"endpoint_id": other.id})).unwrap_err();
    assert!(
        refused.to_string().contains("restate data_destination"),
        "{refused}"
    );
    let moved = patch(
        &first,
        json!({"endpoint_id": other.id, "data_destination": "operator_network"}),
    )
    .unwrap()
    .unwrap();
    assert_eq!(moved.data_destination, "operator_network");
    // A second model on a server nobody declared cannot declare it alone.
    let undeclared = endpoint_of(&store, "undeclared", "openai");
    model_on(&store, "undeclared-a", &undeclared, "unknown");
    let alone: CreateModelProfile = serde_json::from_value(json!({
        "name": "undeclared-b", "endpoint_id": undeclared.id, "model_id": "x",
        "context_tokens": 8192, "max_output_tokens": 1024, "uses": ["durable"],
        "wake_policy": "never", "data_destination": "operator_network"
    }))
    .unwrap();
    let refused = store.create_model_profile(&alone).unwrap_err();
    assert!(
        refused.to_string().contains("not declared yet"),
        "{refused}"
    );
}

#[test]
fn repointing_or_rekeying_a_server_follows_the_route_rules() {
    let (_directory, store) = store();
    let (endpoint, model, _) = configured(&store);
    let patch = |body| {
        store
            .patch_model_profile(&model.id, &serde_json::from_value(body).unwrap())
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        patch(json!({"data_destination":"operator_network"})).data_destination,
        "operator_network"
    );
    assert_eq!(
        patch(json!({"max_output_tokens":1024})).data_destination,
        "operator_network"
    );
    let mut input = CreateEndpoint {
        name: "rename".into(),
        kind: endpoint.kind,
        base_url: endpoint.base_url,
        api_key: "private-key".into(),
        managed_identity: Default::default(),
    };
    store.update_endpoint(&endpoint.id, &input).unwrap();
    assert_eq!(
        store
            .model_profile(&model.id)
            .unwrap()
            .unwrap()
            .data_destination,
        "operator_network"
    );
    // Rotating the key moves nothing: same kind, same address, same place the
    // data goes. It used to reset every profile on the endpoint.
    input.api_key = "rotated-key".into();
    store.update_endpoint(&endpoint.id, &input).unwrap();
    assert_eq!(
        store
            .model_profile(&model.id)
            .unwrap()
            .unwrap()
            .data_destination,
        "operator_network"
    );
    // A new address is a new route: unknown until declared again, and the
    // server's models keep inheriting.
    input.base_url = "http://localhost:9090/v1".into();
    let repointed = store
        .update_endpoint(&endpoint.id, &input)
        .unwrap()
        .unwrap();
    assert_eq!(repointed.data_destination.as_deref(), Some("unknown"));
    let reset = store.model_profile(&model.id).unwrap().unwrap();
    assert_eq!(reset.data_destination, "unknown");
    assert_eq!(reset.data_destination_source, "endpoint");
    // Re-pointing and declaring in one save leaves no unknown in between.
    input.base_url = "http://localhost:9191/v1".into();
    store
        .update_endpoint_with_destination(&endpoint.id, &input, Some("external"))
        .unwrap();
    assert_eq!(
        store
            .model_profile(&model.id)
            .unwrap()
            .unwrap()
            .data_destination,
        "external"
    );
    // A gateway route still states its own destination when it is re-pointed.
    let gateway = endpoint_of(&store, "gateway", "litellm");
    let route = model_on(&store, "route", &gateway, "operator_network");
    let refused = store
        .patch_model_profile(
            &route.id,
            &serde_json::from_value(json!({"model_id":"different"})).unwrap(),
        )
        .unwrap_err();
    assert!(
        refused.to_string().contains("restate data_destination"),
        "{refused}"
    );
    let unchanged = store.model_profile(&route.id).unwrap().unwrap();
    assert_eq!(unchanged.data_destination, "operator_network");
    let restated = store
        .patch_model_profile(
            &route.id,
            &serde_json::from_value(json!({"model_id":"confirmed","data_destination":"external"}))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(restated.data_destination, "external");
    assert!(
        store
            .update_endpoint_with_destination(
                &gateway.id,
                &CreateEndpoint {
                    name: "gateway".into(),
                    kind: "litellm".into(),
                    base_url: "http://gateway.invalid/v1".into(),
                    api_key: String::new(),
                    managed_identity: Default::default(),
                },
                Some("external"),
            )
            .is_err()
    );
}

#[test]
fn idempotency_replays_identical_input_and_rejects_different_input() {
    let (_directory, store) = store();
    let (_, model, project) = configured(&store);
    let input = SubmitJob {
        source: None,
        idempotency_key: "same".into(),
        policy: Default::default(),
        project_id: project.id,
        model_profile_id: model.id,
        harness_profile_id: "pi".into(),
        prompt: "change it".into(),
    };
    let first = store.submit_job(&input).unwrap();
    assert_eq!(first.id, store.submit_job(&input).unwrap().id);
    let changed = SubmitJob {
        source: None,
        prompt: "different".into(),
        ..input
    };
    assert!(
        store
            .submit_job(&changed)
            .unwrap_err()
            .to_string()
            .contains("idempotency")
    );
}

#[test]
fn runner_claim_is_atomic_and_completion_is_durable() {
    let (_directory, store) = store();
    let (_, model, project) = configured(&store);
    let job = store
        .submit_job(&SubmitJob {
            source: None,
            idempotency_key: "claim".into(),
            policy: Default::default(),
            project_id: project.id,
            model_profile_id: model.id,
            harness_profile_id: "pi".into(),
            prompt: "change it".into(),
        })
        .unwrap();
    store
        .set_job_status(&job.id, "ready", "ready", "ready")
        .unwrap();
    assert_eq!(
        store.claim_runner_job(None).unwrap().as_deref(),
        Some(job.id.as_str())
    );
    assert!(store.claim_runner_job(None).unwrap().is_none());
    store
        .complete_job(
            &job.id,
            &RunnerResult {
                patches: vec![RootPatch {
                    root: "repo".into(),
                    base_revision: "abc".into(),
                    patch: "diff".into(),
                }],
                output: "ok".into(),
                provenance: json!({"model":"model"}),
                cleanup: json!({"done":true}),
            },
        )
        .unwrap();
    assert_eq!(store.job(&job.id).unwrap().unwrap().status, "completed");
    assert!(store.job(&job.id).unwrap().unwrap().result.is_some());
    assert!(
        store.list_jobs(10).unwrap()[0].result.is_none(),
        "job lists must not carry retained patches or output"
    );
}

#[test]
fn claim_stores_the_runner_reported_wall_budget_and_deadline() {
    let (_directory, store) = store();
    let (_, model, project) = configured(&store);
    let submit = |key: &str| {
        store
            .submit_job(&SubmitJob {
                source: None,
                idempotency_key: key.into(),
                policy: Default::default(),
                project_id: project.id.clone(),
                model_profile_id: model.id.clone(),
                harness_profile_id: "pi".into(),
                prompt: "change it".into(),
            })
            .unwrap()
    };
    let known = submit("wall-known");
    store
        .set_job_status(&known.id, "ready", "ready", "ready")
        .unwrap();
    // Before any runner claims it, the wall budget is unknown — never guessed.
    assert_eq!(store.job_wall_budget(&known.id).unwrap(), (None, None));
    let before_claim = chrono::Utc::now();
    assert_eq!(
        store.claim_runner_job(Some(1800)).unwrap().as_deref(),
        Some(known.id.as_str())
    );
    let (wall_seconds, deadline_at) = store.job_wall_budget(&known.id).unwrap();
    assert_eq!(wall_seconds, Some(1800));
    let deadline = chrono::DateTime::parse_from_rfc3339(&deadline_at.unwrap()).unwrap();
    // Anchored on the claim timestamp, not some other clock.
    let delta = (deadline.with_timezone(&chrono::Utc) - before_claim).num_seconds();
    assert!((1798..=1802).contains(&delta), "deadline drifted: {delta}s");

    // A runner that does not report its wall budget (an older build, or an
    // omitted field) leaves it unknown rather than inventing a value.
    let unreported = submit("wall-unreported");
    store
        .set_job_status(&unreported.id, "ready", "ready", "ready")
        .unwrap();
    assert_eq!(
        store.claim_runner_job(None).unwrap().as_deref(),
        Some(unreported.id.as_str())
    );
    assert_eq!(store.job_wall_budget(&unreported.id).unwrap(), (None, None));
}

#[test]
fn occupying_jobs_are_preparing_ready_or_running() {
    let (_directory, store) = store();
    let (_, model, project) = configured(&store);
    let job = store
        .submit_job(&SubmitJob {
            source: None,
            idempotency_key: "occupy".into(),
            policy: Default::default(),
            project_id: project.id,
            model_profile_id: model.id,
            harness_profile_id: "pi".into(),
            prompt: "change it".into(),
        })
        .unwrap();
    assert!(store.occupying_jobs().unwrap().is_empty());
    store
        .set_job_status(&job.id, "preparing", "runtime_starting", "preparing")
        .unwrap();
    assert_eq!(store.occupying_jobs().unwrap()[0].id, job.id);
}

#[test]
fn inventory_survives_disconnect_restart_and_legacy_heartbeats() {
    let (directory, store) = store();
    let report =
        json!({"schema_version":1,"collected_at":"2026-09-08T12:00:00Z","gpus":[{"vendor":"AMD"}]});
    let node = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "inventory-node".into(),
            platform: "linux".into(),
            capabilities: json!({"inventory":report,"runtimes":["ready"]}),
        })
        .unwrap();
    store
        .heartbeat_node(&node.node_id, "linux", &json!({"runtimes":["legacy"]}))
        .unwrap();
    assert_eq!(
        store.node(&node.node_id, 30).unwrap().unwrap().capabilities["inventory"],
        report
    );
    store
        .node_maintenance(&node.node_id, &node.credential, true)
        .unwrap();
    store
        .node_maintenance(&node.node_id, &node.credential, false)
        .unwrap();
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE nodes SET last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&node.node_id],
        )
        .unwrap();
    let offline = store.node(&node.node_id, 30).unwrap().unwrap();
    assert_eq!(offline.status, "offline");
    assert_eq!(offline.capabilities["inventory"], report);
    let token = store
        .create_enrollment_token(15, Some("inventory-node"))
        .unwrap();
    store
        .enroll_node(&EnrollNode {
            token,
            name: "inventory-node".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    assert_eq!(
        store.node(&node.node_id, 30).unwrap().unwrap().capabilities["inventory"],
        report
    );
    let updated = json!({"schema_version":1,"collected_at":"2026-09-08T13:00:00Z","gpus":[]});
    store
        .heartbeat_node(&node.node_id, "linux", &json!({"inventory":updated}))
        .unwrap();
    assert_eq!(
        store.node(&node.node_id, 30).unwrap().unwrap().capabilities["inventory"],
        updated
    );
    // Reopening the actual database preserves the latest report.
    drop(store);
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let reopened = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    assert_eq!(
        reopened
            .node(&node.node_id, 30)
            .unwrap()
            .unwrap()
            .capabilities["inventory"],
        updated
    );
}

#[test]
fn enrollment_tokens_are_single_use_and_actions_are_typed() {
    let (_directory, store) = store();
    let token = store.create_enrollment_token(15, None).unwrap();
    let request = EnrollNode {
        token: token.clone(),
        name: "worker".into(),
        platform: "linux".into(),
        capabilities: json!({"runtimes":["local"]}),
    };
    let enrolled = store.enroll_node(&request).unwrap();
    assert!(
        store
            .authenticate_node(&enrolled.node_id, &enrolled.credential)
            .unwrap()
    );
    assert!(store.enroll_node(&request).is_err());
    let action = store
        .enqueue_action(
            &enrolled.node_id,
            None,
            "ensure",
            json!({"runtime_profile":"local"}),
        )
        .unwrap();
    assert_eq!(
        store
            .poll_node_action(&enrolled.node_id)
            .unwrap()
            .unwrap()
            .id,
        action.id
    );
    assert!(
        store
            .complete_node_action(&enrolled.node_id, &action.id, true, json!({"ready":true}))
            .unwrap()
    );
}

#[test]
fn reenrollment_preserves_node_identity_and_rotates_the_credential() {
    let (directory, store) = store();
    let first = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "worker".into(),
            platform: "linux".into(),
            capabilities: json!({"runtimes":["old"]}),
        })
        .unwrap();
    let stale = store
        .enqueue_action(
            &first.node_id,
            None,
            "ensure",
            json!({"runtime_profile":"old"}),
        )
        .unwrap();
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE nodes SET last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&first.node_id],
        )
        .unwrap();
    let second = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, Some("worker")).unwrap(),
            name: "worker".into(),
            platform: "linux".into(),
            capabilities: json!({"runtimes":["new"]}),
        })
        .unwrap();

    assert_eq!(first.node_id, second.node_id);
    assert!(
        !store
            .authenticate_node(&first.node_id, &first.credential)
            .unwrap()
    );
    assert!(
        store
            .authenticate_node(&second.node_id, &second.credential)
            .unwrap()
    );
    assert_eq!(store.action(&stale.id).unwrap().unwrap().status, "failed");
}

#[test]
fn live_enrollment_keeps_the_token_and_survives_heartbeats() {
    let (directory, store) = store();
    let first = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "worker".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let token = store.create_enrollment_token(15, Some("worker")).unwrap();
    let request = EnrollNode {
        token: token.clone(),
        name: "worker".into(),
        platform: "linux".into(),
        capabilities: json!({}),
    };
    let beating = std::thread::spawn({
        let store = store.clone();
        let id = first.node_id.clone();
        move || {
            for _ in 0..40 {
                store.heartbeat_node(&id, "linux", &json!({})).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    });
    for _ in 0..8 {
        let error = store.enroll_node(&request).unwrap_err();
        assert!(
            error.to_string().contains("already enrolled and reachable"),
            "{error:#}"
        );
    }
    beating.join().unwrap();
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE nodes SET last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&first.node_id],
        )
        .unwrap();
    let second = store.enroll_node(&request).unwrap();
    assert_eq!(second.node_id, first.node_id);
}

#[test]
fn api_tokens_are_named_scoped_hashed_and_revocable() {
    let (directory, store) = store();
    let issued = store
        .create_api_token(&CreateApiToken {
            name: "Test editor".into(),
            permissions: vec!["models:read".into()],
        })
        .unwrap();
    assert!(
        store
            .api_token_has_permission(&issued.token, "models:read")
            .unwrap()
    );
    let principal = store
        .authenticate_api_token(&issued.token, "models:read")
        .unwrap()
        .unwrap();
    assert_eq!(principal.id, issued.details.id);
    assert_eq!(principal.name, "Test editor");
    assert_eq!(principal.permissions, vec!["models:read"]);
    assert!(
        !store
            .api_token_has_permission(&issued.token, "inference:invoke")
            .unwrap()
    );
    let database = std::fs::read(directory.path().join("state.sqlite3")).unwrap();
    assert!(
        !database
            .windows(issued.token.len())
            .any(|part| part == issued.token.as_bytes())
    );
    assert!(store.revoke_api_token(&issued.details.id).unwrap());
    assert!(
        !store
            .api_token_has_permission(&issued.token, "models:read")
            .unwrap()
    );
    assert!(
        store
            .authenticate_api_token(&issued.token, "models:read")
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_tokens_last_use_is_refreshed_once_per_window_not_per_request() {
    let (directory, store) = store();
    let issued = store
        .create_api_token(&CreateApiToken {
            name: "Busy client".into(),
            permissions: vec!["inference:invoke".into()],
        })
        .unwrap();
    let last_used = || store.list_api_tokens().unwrap()[0].last_used_at.clone();
    assert_eq!(last_used(), None);

    store
        .authenticate_api_token(&issued.token, "inference:invoke")
        .unwrap()
        .unwrap();
    let first = last_used().expect("the first use is recorded");
    std::thread::sleep(std::time::Duration::from_millis(20));
    store
        .authenticate_api_token(&issued.token, "inference:invoke")
        .unwrap()
        .unwrap();
    assert_eq!(last_used(), Some(first), "no write inside the window");

    let stale = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE api_tokens SET last_used_at=?1 WHERE id=?2",
            rusqlite::params![stale, issued.details.id],
        )
        .unwrap();
    store
        .authenticate_api_token(&issued.token, "inference:invoke")
        .unwrap()
        .unwrap();
    assert!(last_used().unwrap() > stale, "a stale value is refreshed");
}

#[test]
fn a_single_node_lookup_matches_its_row_in_the_listing() {
    let (directory, store) = store();
    let ids: Vec<String> = ["ready", "silent", "paused", "gone"]
        .iter()
        .map(|name| {
            store
                .enroll_node(&EnrollNode {
                    token: store.create_enrollment_token(15, None).unwrap(),
                    name: (*name).into(),
                    platform: "linux".into(),
                    capabilities: json!({"inventory":{"name":name}}),
                })
                .unwrap()
                .node_id
        })
        .collect();
    let database = rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap();
    database
        .execute(
            "UPDATE nodes SET last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&ids[1]],
        )
        .unwrap();
    store.set_node_maintenance(&ids[2], None, true).unwrap();
    store.remove_node(&ids[3]).unwrap();

    let listed = store.list_nodes(15).unwrap();
    assert_eq!(listed.len(), 4);
    for node in &listed {
        let found = store.node(&node.id, 15).unwrap().expect("listed node");
        assert_eq!(json!(found), json!(node), "{}", node.name);
    }
    let status = |name: &str| {
        listed
            .iter()
            .find(|node| node.name == name)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(
        ["ready", "silent", "paused", "gone"].map(status),
        ["online", "offline", "maintenance", "revoked"]
    );
    assert!(store.node("no-such-node", 15).unwrap().is_none());
}

#[test]
fn routing_revision_changes_only_with_the_route() {
    let (_directory, store) = store();
    let (endpoint, model, _) = configured(&store);
    let first = store.routing_revision(&model).unwrap();
    assert_eq!(first, store.routing_revision(&model).unwrap());

    let relabelled = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"max_output_tokens":2048,"model_label":"Label"}))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(first, store.routing_revision(&relabelled).unwrap());

    let resaved = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"model_id":"another-model"})).unwrap(),
        )
        .unwrap()
        .unwrap();
    let profile_revision = store.routing_revision(&resaved).unwrap();
    assert_ne!(first, profile_revision);
    assert_eq!(profile_revision, store.routing_revision(&resaved).unwrap());

    store
        .update_endpoint(
            &endpoint.id,
            &CreateEndpoint {
                name: endpoint.name,
                kind: endpoint.kind,
                base_url: endpoint.base_url,
                api_key: "replacement-key".into(),
                managed_identity: Default::default(),
            },
        )
        .unwrap();
    let current = store.model_profile(&model.id).unwrap().unwrap();
    assert_ne!(profile_revision, store.routing_revision(&current).unwrap());
}

#[test]
fn last_check_is_persisted_without_secrets() {
    let (_directory, store) = store();
    let (endpoint, model, _) = configured(&store);
    assert!(
        store
            .record_endpoint_check(&endpoint.id, true, "Listed models")
            .unwrap()
    );
    let listed = store.list_endpoints().unwrap();
    assert_eq!(listed[0].last_check.last_check_ok, Some(true));
    assert_eq!(
        listed[0].last_check.last_check_message.as_deref(),
        Some("Listed models")
    );
    assert!(
        store
            .record_model_check(&model.id, false, "Check failed")
            .unwrap()
    );
    let models = store.list_model_profiles().unwrap();
    assert_eq!(models[0].last_check.last_check_ok, Some(false));
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("private-key")
    );
}

fn submit(store: &Store) -> Job {
    let (_, model, project) = configured(store);
    store
        .submit_job(&SubmitJob {
            source: None,
            idempotency_key: "regression".into(),
            policy: Default::default(),
            project_id: project.id,
            model_profile_id: model.id,
            harness_profile_id: "pi".into(),
            prompt: "change it".into(),
        })
        .unwrap()
}

#[test]
fn accepted_configuration_survives_edits_and_restart_without_plaintext_secrets() {
    let (directory, store) = store();
    let job = submit(&store);
    let package = store.runner_package(&job.id).unwrap().unwrap();
    store
        .update_endpoint(
            &package.model.endpoint_id,
            &CreateEndpoint {
                name: package.endpoint.endpoint.name.clone(),
                kind: package.endpoint.endpoint.kind.clone(),
                base_url: "https://changed.invalid/v1".into(),
                api_key: package.endpoint.api_key.clone(),
                managed_identity: Default::default(),
            },
        )
        .unwrap();
    store
        .patch_model_profile(
            &job.model_profile_id,
            &serde_json::from_value(json!({
                "model_id":"different", "context_tokens":16384
            }))
            .unwrap(),
        )
        .unwrap();
    store
        .patch_project(
            job.project_id.as_deref().unwrap(),
            &PatchProject {
                instruction_files: Some(vec!["new-policy.md".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    drop(store);
    let db = directory.path().join("state.sqlite3");
    let bytes = std::fs::read(&db).unwrap();
    assert!(
        !bytes
            .windows(b"private-key".len())
            .any(|part| part == b"private-key")
    );
    let reopened = Store::open(
        &db,
        SecretBox::load_or_create(&directory.path().join("master-key")).unwrap(),
    )
    .unwrap();
    let frozen = reopened.runner_package(&job.id).unwrap().unwrap();
    assert_eq!(frozen.model.model_id, "model");
    assert_eq!(frozen.model.context_tokens, 8192);
    assert_eq!(
        frozen.endpoint.endpoint.base_url,
        "http://localhost:8080/v1"
    );
    assert_eq!(frozen.endpoint.api_key, "private-key");
    assert!(frozen.project.instruction_files.is_empty());
}

#[test]
fn ready_cancellation_remains_visible_to_scheduler_and_cannot_be_claimed() {
    let (_directory, store) = store();
    let job = submit(&store);
    store
        .set_job_status(&job.id, "ready", "ready", "ready")
        .unwrap();
    assert_eq!(store.active_jobs().unwrap()[0].id, job.id);
    store.request_cancel(&job.id).unwrap();
    assert!(store.claim_runner_job(None).unwrap().is_none());
    assert_eq!(store.schedulable_jobs().unwrap()[0].id, job.id);
}

#[test]
fn concurrent_replays_accept_exactly_one_job_and_one_event() {
    let (_directory, store) = store();
    let (_, model, project) = configured(&store);
    let input = serde_json::to_string(&SubmitJob {
        source: None,
        idempotency_key: "concurrent".into(),
        policy: Default::default(),
        project_id: project.id,
        model_profile_id: model.id,
        harness_profile_id: "pi".into(),
        prompt: "change it".into(),
    })
    .unwrap();
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            let input = input.clone();
            std::thread::spawn(move || {
                store
                    .submit_job(&serde_json::from_str(&input).unwrap())
                    .unwrap()
                    .id
            })
        })
        .collect();
    let ids: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(store.list_jobs(100).unwrap().len(), 1);
    assert_eq!(store.events(&ids[0], 0).unwrap().len(), 1);
}

#[test]
fn lease_expiry_cannot_be_revived_by_a_late_heartbeat_or_completion() {
    let (directory, store) = store();
    let job = submit(&store);
    store
        .set_job_status(&job.id, "ready", "ready", "ready")
        .unwrap();
    store.claim_runner_job(None).unwrap();
    assert!(!store.runner_heartbeat(&job.id).unwrap());
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE jobs SET runner_lease_expires_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&job.id],
        )
        .unwrap();
    assert!(store.runner_heartbeat(&job.id).unwrap());
    assert!(store.runner_heartbeat("missing").unwrap());
    assert!(
        store
            .complete_job(
                &job.id,
                &RunnerResult {
                    patches: vec![],
                    output: "late".into(),
                    provenance: json!({}),
                    cleanup: json!({}),
                }
            )
            .is_err()
    );
    store.fail_expired_runners().unwrap();
    assert!(store.runner_heartbeat(&job.id).unwrap());
    assert_eq!(store.job(&job.id).unwrap().unwrap().status, "failed");
}

#[test]
fn cancellation_wins_a_fast_harness_exit_and_identical_result_replay() {
    for failed in [false, true] {
        let (_directory, store) = store();
        let job = submit(&store);
        store
            .set_job_status(&job.id, "ready", "ready", "ready")
            .unwrap();
        store.claim_runner_job(None).unwrap();
        store.request_cancel(&job.id).unwrap();
        let result = RunnerResult {
            patches: vec![],
            output: "retained evidence".into(),
            provenance: json!({}),
            cleanup: json!({"containers_removed":true}),
        };
        for _ in 0..2 {
            if failed {
                store
                    .fail_job_with_result(
                        &job.id,
                        json!({"code":"model_request_failed"}),
                        false,
                        Some(&result),
                    )
                    .unwrap();
            } else {
                store.complete_job(&job.id, &result).unwrap();
            }
        }
        let saved = store.job(&job.id).unwrap().unwrap();
        assert_eq!(saved.status, "cancelled");
        assert_eq!(saved.error.unwrap()["code"], "cancelled_or_lease_lost");
        assert_eq!(saved.result.unwrap()["output"], "retained evidence");
        assert!(
            store
                .fail_job(&job.id, json!({"different":true}), false)
                .is_err()
        );
    }
}

#[test]
fn supplied_source_is_encrypted_separate_and_available_only_with_a_live_lease() {
    use base64::Engine;
    use poundlock::source::{JobSource, SourceFile};
    let (directory, store) = store();
    let (_, model, _) = configured(&store);
    let source = JobSource::Snapshot {
        files: vec![SourceFile {
            path: "file.txt".into(),
            content_base64: base64::engine::general_purpose::STANDARD
                .encode("private source sentinel"),
            executable: false,
        }],
    };
    let input = SubmitJob {
        project_id: String::new(),
        source: Some(source.clone()),
        idempotency_key: "snapshot".into(),
        policy: Default::default(),
        model_profile_id: model.id,
        harness_profile_id: "pi".into(),
        prompt: "Change it".into(),
    };
    let job = store.submit_job(&input).unwrap();
    assert!(job.project_id.is_none());
    assert_eq!(job.id, store.submit_job(&input).unwrap().id);
    let package = store.runner_package(&job.id).unwrap().unwrap();
    assert_eq!(package.source, Some(source.summary()));
    assert!(
        !serde_json::to_string(&package)
            .unwrap()
            .contains("content_base64")
    );
    assert!(store.runner_source(&job.id).unwrap().is_none());
    store
        .set_job_status(&job.id, "ready", "ready", "Ready")
        .unwrap();
    assert_eq!(store.claim_runner_job(None).unwrap(), Some(job.id.clone()));
    assert_eq!(
        store.runner_source(&job.id).unwrap().unwrap().summary(),
        source.summary()
    );
    let db = rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap();
    let ciphertext: String = db
        .query_row(
            "SELECT source_ciphertext FROM jobs WHERE id=?1",
            [&job.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!ciphertext.contains("file.txt"));
    store.request_cancel(&job.id).unwrap();
    assert!(store.runner_source(&job.id).unwrap().is_none());
}

/// A job whose source carries a Git read token, as a developer would send it.
fn git_job(model: &ModelProfile, key: &str) -> SubmitJob {
    SubmitJob {
        project_id: String::new(),
        source: Some(poundlock::source::JobSource::Git {
            repository: "https://github.com/owner/repo.git".into(),
            revision: "a".repeat(40),
            credentials: Some(poundlock::git_source::GitCredentials {
                username: "reader".into(),
                token: "private-git-secret".into(),
            }),
        }),
        idempotency_key: key.into(),
        policy: Default::default(),
        model_profile_id: model.id.clone(),
        harness_profile_id: "pi".into(),
        prompt: "change it".into(),
    }
}

fn source_stored(directory: &tempfile::TempDir, id: &str) -> bool {
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT source_ciphertext IS NOT NULL FROM jobs WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn every_way_a_job_ends_removes_its_source() {
    let (directory, store) = store();
    let (_, model, _) = configured(&store);
    let result = RunnerResult {
        patches: vec![],
        output: "done".into(),
        provenance: json!({}),
        cleanup: json!({}),
    };
    let claimed = |key: &str| {
        let job = store.submit_job(&git_job(&model, key)).unwrap();
        store
            .set_job_status(&job.id, "ready", "ready", "Ready")
            .unwrap();
        assert_eq!(store.claim_runner_job(None).unwrap(), Some(job.id.clone()));
        assert!(store.runner_source(&job.id).unwrap().is_some());
        job
    };

    let completed = claimed("completed");
    store.complete_job(&completed.id, &result).unwrap();
    let failed = claimed("failed");
    store
        .fail_job(&failed.id, json!({"code":"harness_failed"}), false)
        .unwrap();
    let cancelled = claimed("cancelled");
    store.request_cancel(&cancelled.id).unwrap();
    store.complete_job(&cancelled.id, &result).unwrap();
    let unclaimed = store.submit_job(&git_job(&model, "unclaimed")).unwrap();
    store
        .fail_job(&unclaimed.id, json!({"code":"cancelled_before_run"}), true)
        .unwrap();
    let lost = claimed("lost");
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE jobs SET runner_lease_expires_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&lost.id],
        )
        .unwrap();
    assert_eq!(store.fail_expired_runners().unwrap(), vec![lost.id.clone()]);
    let waiting = store.submit_job(&git_job(&model, "waiting")).unwrap();

    for (job, status) in [
        (&completed, "completed"),
        (&failed, "failed"),
        (&cancelled, "cancelled"),
        (&unclaimed, "cancelled"),
        (&lost, "failed"),
    ] {
        assert_eq!(store.job(&job.id).unwrap().unwrap().status, status);
        assert!(
            !source_stored(&directory, &job.id),
            "{status} kept its source"
        );
        // Replaying the submission still returns the finished job.
        let key = job.idempotency_key.as_str();
        assert_eq!(store.submit_job(&git_job(&model, key)).unwrap().id, job.id);
    }
    assert!(source_stored(&directory, &waiting.id));
    // An ending is recorded with its result, which is what removes the source.
    for status in ["completed", "failed", "cancelled"] {
        assert!(store.set_job_status(&waiting.id, status, "x", "x").is_err());
    }
    assert!(source_stored(&directory, &waiting.id));
}

#[test]
fn opening_the_store_removes_sources_kept_by_jobs_that_already_ended() {
    let (directory, store) = store();
    let (_, model, _) = configured(&store);
    let ended = store.submit_job(&git_job(&model, "ended")).unwrap();
    let waiting = store.submit_job(&git_job(&model, "waiting")).unwrap();
    // How a job ended before endings removed the source.
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE jobs SET status='completed' WHERE id=?1",
            [&ended.id],
        )
        .unwrap();
    drop(store);
    assert!(source_stored(&directory, &ended.id));

    let store = reopen(&directory);
    assert!(!source_stored(&directory, &ended.id));
    assert!(source_stored(&directory, &waiting.id));
    assert_eq!(
        store.submit_job(&git_job(&model, "ended")).unwrap().id,
        ended.id
    );
}

#[test]
fn a_token_holds_a_bounded_number_of_unfinished_jobs() {
    let (_directory, store) = store();
    let (_, model, _) = configured(&store);
    let token = |name: &str| {
        store
            .create_api_token(&CreateApiToken {
                name: name.into(),
                permissions: vec!["jobs:submit".into()],
            })
            .unwrap()
            .details
            .id
    };
    let (busy, other) = (token("busy"), token("other"));
    let submit =
        |key: &str, token: Option<&str>| store.submit_job_for(&git_job(&model, key), token, 2);
    let first = submit("first", Some(&busy)).unwrap();
    submit("second", Some(&busy)).unwrap();

    let refused = submit("third", Some(&busy)).unwrap_err();
    let limit = refused
        .downcast_ref::<poundlock::store::TooManyActiveJobsError>()
        .expect("a typed refusal");
    assert_eq!(limit.0, 2);
    assert_eq!(store.list_jobs_for(10, Some(&busy)).unwrap().len(), 2);

    // A replay is the job already counted, not a new one.
    assert_eq!(submit("first", Some(&busy)).unwrap().id, first.id);
    // Other tokens and the operator have their own room.
    submit("elsewhere", Some(&other)).unwrap();
    submit("operator", None).unwrap();

    // A finished job no longer counts.
    store
        .fail_job(&first.id, json!({"code":"cancelled_before_run"}), true)
        .unwrap();
    submit("third", Some(&busy)).unwrap();
    assert!(submit("fourth", Some(&busy)).is_err());
}

#[test]
fn old_required_project_schema_migrates_without_losing_jobs_or_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(&include_str!("../migrations/schema.sql").replace(
        "project_id TEXT REFERENCES",
        "project_id TEXT NOT NULL REFERENCES",
    ))
    .unwrap();
    drop(db);
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&path, secrets.clone()).unwrap();
    let (_, model, project) = configured(&store);
    let job = store
        .submit_job(&SubmitJob {
            project_id: project.id,
            source: None,
            idempotency_key: "legacy".into(),
            policy: Default::default(),
            model_profile_id: model.id,
            harness_profile_id: "pi".into(),
            prompt: "change".into(),
        })
        .unwrap();
    drop(store);
    // Rebuild the populated jobs table with the old NOT NULL constraint.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.pragma_update(None, "foreign_keys", false).unwrap();
    db.execute_batch(&include_str!("../migrations/job-source.sql").replace(
        "project_id TEXT REFERENCES",
        "project_id TEXT NOT NULL REFERENCES",
    ))
    .unwrap();
    drop(db);
    let store = Store::open(&path, secrets).unwrap();
    assert_eq!(store.job(&job.id).unwrap().unwrap().prompt, "change");
    assert_eq!(store.events(&job.id, 0).unwrap().len(), 1);
    assert!(store.runner_package(&job.id).unwrap().is_some());
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT \"notnull\" FROM pragma_table_info('jobs') WHERE name='project_id'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn ending_a_runtime_lease_fences_only_unclaimed_work_and_late_renewals() {
    let (directory, store) = store();
    let token = store.create_enrollment_token(15, None).unwrap();
    let node = store
        .enroll_node(&EnrollNode {
            token,
            name: "runtime-fencing".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let action = |kind: &str, lease: &str| {
        store
            .enqueue_action(&node.node_id, None, kind, json!({"lease_id":lease}))
            .unwrap()
    };
    let running = action("ensure", "running");
    assert_eq!(
        store.poll_node_action(&node.node_id).unwrap().unwrap().id,
        running.id
    );
    let abandoned = action("ensure", "abandoned");
    let renewal = action("renew", "abandoned");
    let other = action("ensure", "other");
    action("release", "abandoned");
    action("release", "running");
    for id in [&abandoned.id, &renewal.id] {
        assert_eq!(store.action(id).unwrap().unwrap().status, "failed");
    }
    assert_eq!(
        store.action(&running.id).unwrap().unwrap().status,
        "running"
    );
    assert_eq!(store.action(&other.id).unwrap().unwrap().status, "pending");
    assert_eq!(action("renew", "abandoned").status, "failed");
    assert_eq!(action("ensure", "abandoned").status, "failed");
    // Simulate an older controller's queued action or an interrupted running action.
    rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap().execute(
        "UPDATE node_actions SET status='running',updated_at='2000-01-01T00:00:00Z' WHERE id=?1", [&abandoned.id]).unwrap();
    assert_eq!(
        store.poll_node_action(&node.node_id).unwrap().unwrap().id,
        other.id
    );
    assert_eq!(
        store.action(&abandoned.id).unwrap().unwrap().status,
        "failed"
    );
    assert!(
        store
            .complete_node_action(&node.node_id, &running.id, true, json!({}))
            .unwrap()
    );
}

#[test]
fn node_maintenance_and_revocation_fence_work_and_credentials() {
    let (_directory, store) = store();
    let enrolled = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "linux-node".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let id = &enrolled.node_id;
    let payload = json!({"lease_id":"active-lease","runtime_profile":"ready"});
    let ensure = store
        .enqueue_action(id, None, "ensure", payload.clone())
        .unwrap();
    assert!(store.node_active(id).unwrap());
    assert!(
        store
            .node_maintenance(id, &enrolled.credential, true)
            .is_err()
    );
    assert!(store.revoke_node(id, &enrolled.credential).is_err());
    assert_eq!(store.poll_node_action(id).unwrap().unwrap().id, ensure.id);
    store
        .complete_node_action(id, &ensure.id, true, json!({}))
        .unwrap();
    let release = store
        .enqueue_action(id, None, "release", payload.clone())
        .unwrap();
    assert!(
        store.revoke_node(id, &enrolled.credential).is_err(),
        "Unacknowledged release still holds work"
    );
    assert_eq!(store.poll_node_action(id).unwrap().unwrap().id, release.id);
    store
        .complete_node_action(id, &release.id, true, json!({}))
        .unwrap();
    assert!(!store.node_active(id).unwrap());
    store
        .node_maintenance(id, &enrolled.credential, true)
        .unwrap();
    store.heartbeat_node(id, "linux", &json!({})).unwrap();
    assert_eq!(store.node(id, 15).unwrap().unwrap().status, "maintenance");
    assert_eq!(
        store
            .enqueue_action(id, None, "ensure", json!({"lease_id":"blocked"}))
            .unwrap()
            .status,
        "failed"
    );
    assert!(store.poll_node_action(id).unwrap().is_none());
    store
        .node_maintenance(id, &enrolled.credential, false)
        .unwrap();
    assert!(store.revoke_node(id, "wrong-credential").is_err());
    store.revoke_node(id, &enrolled.credential).unwrap();
    assert!(!store.authenticate_node(id, &enrolled.credential).unwrap());
    store.heartbeat_node(id, "linux", &json!({})).unwrap();
    assert_eq!(store.node(id, 15).unwrap().unwrap().status, "revoked");
    assert_eq!(
        store
            .enqueue_action(id, None, "ensure", json!({"lease_id":"after-removal"}))
            .unwrap()
            .status,
        "failed"
    );
    let renewed = store
        .enroll_node(&EnrollNode {
            token: store
                .create_enrollment_token(15, Some("linux-node"))
                .unwrap(),
            name: "linux-node".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    assert_eq!(renewed.node_id, *id);
    assert!(store.authenticate_node(id, &renewed.credential).unwrap());
    assert!(!store.node_active(id).unwrap());
    assert!(!store.authenticate_node(id, &enrolled.credential).unwrap());
}

#[test]
fn job_checks_are_frozen_persisted_and_part_of_idempotency() {
    let (directory, store) = store();
    let (_, model, project) = configured(&store);
    let mut input: SubmitJob = serde_json::from_value(json!({
        "idempotency_key":"checks-contract","project_id":project.id,
        "model_profile_id":model.id,"harness_profile_id":"pi","prompt":"Fix the calculation.",
        "policy":{"verification":{"commands":[{"command":"python3 -m unittest","timeout_seconds":60}],"protected_paths":["tests"]},"max_repairs":2}
    })).unwrap();
    let job = store.submit_job(&input).unwrap();
    assert_eq!(store.job(&job.id).unwrap().unwrap().policy, input.policy);
    assert_eq!(store.list_jobs(10).unwrap()[0].policy, input.policy);
    assert_eq!(
        store.runner_package(&job.id).unwrap().unwrap().job.policy,
        input.policy
    );
    assert_eq!(store.submit_job(&input).unwrap().id, job.id);
    input.policy.max_repairs = 1;
    assert!(
        store
            .submit_job(&input)
            .unwrap_err()
            .to_string()
            .contains("idempotency")
    );
    assert!(directory.path().exists());
    let legacy: SubmitJob = serde_json::from_value(json!({"idempotency_key":"legacy-default","project_id":project.id,"model_profile_id":model.id,"harness_profile_id":"pi","prompt":"Fix it."})).unwrap();
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("policy")
            .is_none(),
        "default policy must preserve old request hashes"
    );
}

#[test]
fn expired_orphaned_actions_do_not_hold_a_registration_forever() {
    let (directory, store) = store();
    let node = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "old-machine".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let payload = json!({"lease_id":"old-lease", "runtime_profile":"ready"});
    let action = store
        .enqueue_action(&node.node_id, None, "ensure", payload.clone())
        .unwrap();
    store.poll_node_action(&node.node_id).unwrap();
    store
        .complete_node_action(&node.node_id, &action.id, true, json!({}))
        .unwrap();
    assert!(store.node_active(&node.node_id).unwrap());
    let db = rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap();
    db.execute("UPDATE nodes SET credential_issued_at='2000-01-01T00:00:00Z',last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1", [&node.node_id]).unwrap();
    db.execute("UPDATE node_actions SET created_at='2000-01-02T00:00:00Z',updated_at='2000-01-02T00:00:00Z' WHERE id=?1", [&action.id]).unwrap();
    assert!(
        !store.node_active(&node.node_id).unwrap(),
        "Old unrenewed history is not a live lease"
    );
    let renewal = store
        .enqueue_action(&node.node_id, None, "renew", payload)
        .unwrap();
    assert!(
        store.node_active(&node.node_id).unwrap(),
        "Recent renewal still protects the node even when offline"
    );
    assert!(store.remove_node(&node.node_id).is_err());
    db.execute("UPDATE node_actions SET created_at='2000-01-02T00:00:00Z',updated_at='2000-01-02T00:00:00Z' WHERE id=?1", [&renewal.id]).unwrap();
    let (_, model, project) = configured(&store);
    let job = store
        .submit_job(&SubmitJob {
            idempotency_key: "live-job-with-old-action".into(),
            project_id: project.id,
            model_profile_id: model.id,
            harness_profile_id: "pi".into(),
            prompt: "Work".into(),
            source: None,
            policy: Default::default(),
        })
        .unwrap();
    db.execute(
        "UPDATE node_actions SET job_id=?1 WHERE id=?2",
        rusqlite::params![job.id, action.id],
    )
    .unwrap();
    assert!(
        store.node_active(&node.node_id).unwrap(),
        "A current job cannot expire out of the removal guard"
    );
    assert!(store.remove_node(&node.node_id).is_err());
    store
        .fail_job(&job.id, json!({"code":"fixture_finished"}), false)
        .unwrap();
    assert!(store.remove_node(&node.node_id).unwrap());
    assert_eq!(
        store.node(&node.node_id, 15).unwrap().unwrap().status,
        "revoked"
    );
    assert!(
        !store
            .authenticate_node(&node.node_id, &node.credential)
            .unwrap()
    );
    assert!(
        store.action(&action.id).unwrap().is_some(),
        "Historical evidence stays available"
    );
}

#[test]
fn removed_configuration_preserves_accepted_jobs_and_frees_display_names() {
    let (directory, store) = store();
    let job = submit(&store);
    let before = store.runner_package(&job.id).unwrap().unwrap();
    assert!(store.remove_endpoint(&before.model.endpoint_id).is_err());
    assert!(store.remove_model_profile(&before.model.id).unwrap());
    assert!(store.remove_endpoint(&before.model.endpoint_id).unwrap());
    assert!(store.remove_project(&before.project.id).unwrap());
    assert!(store.model_profile(&before.model.id).unwrap().is_none());
    assert!(
        store
            .endpoint_secret(&before.model.endpoint_id)
            .unwrap()
            .is_none()
    );
    assert!(store.list_projects().unwrap().is_empty());
    let replacement = configured(&store);
    assert_ne!(replacement.1.id, before.model.id);
    drop(store);
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    let after = store.runner_package(&job.id).unwrap().unwrap();
    assert_eq!(after.model.name, before.model.name);
    assert_eq!(after.endpoint.api_key, before.endpoint.api_key);
    assert_eq!(after.project.roots, before.project.roots);
    assert_eq!(after.job.id, before.job.id);
}

#[test]
fn model_profile_uses_backfills_from_workload_class_on_legacy_databases() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(&include_str!("../migrations/schema.sql").replace(
        "  revision_nonce TEXT NOT NULL DEFAULT '',\n  uses_json TEXT NOT NULL DEFAULT '[\"durable\"]'\n",
        "  revision_nonce TEXT NOT NULL DEFAULT ''\n",
    ))
    .unwrap();
    db.execute(
        "INSERT INTO endpoints (id,name,kind,base_url,api_key_ciphertext,created_at,revision_nonce)
         VALUES ('endpoint','endpoint','openai','http://localhost/v1','','now','')",
        [],
    )
    .unwrap();
    for (id, class) in [
        ("interactive", "interactive-coding"),
        ("durable", "durable-coding-agent"),
        ("completion", "fast-code-completion"),
        ("unrecognized", "something-else"),
    ] {
        db.execute(
            "INSERT INTO model_profiles
             (id,name,endpoint_id,model_id,context_tokens,max_output_tokens,workload_class,wake_policy,created_at)
             VALUES (?1,?1,'endpoint','model',8192,1024,?2,'never','now')",
            rusqlite::params![id, class],
        )
        .unwrap();
    }
    drop(db);
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&path, secrets).unwrap();
    let profiles = store.list_model_profiles().unwrap();
    let uses = |id: &str| -> Vec<String> {
        profiles
            .iter()
            .find(|profile| profile.id == id)
            .unwrap()
            .uses
            .iter()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(uses("interactive"), vec!["interactive".to_string()]);
    assert_eq!(uses("durable"), vec!["durable".to_string()]);
    assert_eq!(uses("completion"), vec!["completion".to_string()]);
    // Unrecognized legacy values fall back to the same default as a fresh column.
    assert_eq!(uses("unrecognized"), vec!["durable".to_string()]);
}

#[test]
fn thinking_migrates_to_model_default_and_validates_allowed_values() {
    let (directory, store) = store();
    let (endpoint, model, _) = configured(&store);
    assert_eq!(model.thinking, "model_default");
    // Simulate a database written before the additive thinking column.
    drop(store);
    let path = directory.path().join("state.sqlite3");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE model_profiles DROP COLUMN thinking")
        .unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&path, secrets).unwrap();
    assert_eq!(
        store.model_profile(&model.id).unwrap().unwrap().thinking,
        "model_default"
    );
    let patched = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"thinking": "off"})).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(patched.thinking, "off");
    assert_eq!(
        store.model_profile(&model.id).unwrap().unwrap().thinking,
        "off"
    );
    // Patching leaves an omitted `thinking` unchanged.
    let unchanged = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"name": "coding-renamed"})).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.thinking, "off");
    let rejected = store.patch_model_profile(
        &model.id,
        &serde_json::from_value(json!({"thinking": "always"})).unwrap(),
    );
    assert!(rejected.is_err());
    let create_input: CreateModelProfile = serde_json::from_value(json!({
        "name": "bad-thinking",
        "endpoint_id": endpoint.id,
        "model_id": "m",
        "context_tokens": 8192,
        "max_output_tokens": 1024,
        "workload_class": "durable-coding-agent",
        "wake_policy": "never",
        "thinking": "always"
    }))
    .unwrap();
    store.create_model_profile(&create_input).unwrap_err();
    let create_off: CreateModelProfile = serde_json::from_value(json!({
        "name": "good-thinking",
        "endpoint_id": endpoint.id,
        "model_id": "m",
        "context_tokens": 8192,
        "max_output_tokens": 1024,
        "workload_class": "durable-coding-agent",
        "wake_policy": "never",
        "thinking": "off"
    }))
    .unwrap();
    assert_eq!(
        store.create_model_profile(&create_off).unwrap().thinking,
        "off"
    );
}

/// A valid `poundlock_toolkit`-sourced measurement matching `configured()`'s
/// profile (`context_tokens: 8192`, `thinking: "model_default"`).
fn valid_capability(name: &str, context_tokens: u64, reasoning: &str) -> serde_json::Value {
    json!({
        "name": name,
        "status": "qualified",
        "passed": 3,
        "attempted": 3,
        "source": "poundlock_toolkit",
        "toolkit_version": "0.4.0",
        "run_ids": ["run-1", "run-2", "run-3"],
        "identity_sha256": "a".repeat(64),
        "result_sha256": "b".repeat(64),
        "measured_at": "2026-08-01T00:00:00Z",
        "measured_settings": {
            "context_tokens": context_tokens,
            "max_output_tokens": 4096,
            "reasoning": reasoning,
            "harness": "pi"
        },
        "note": "measured on orion",
        // Client-supplied recorded_at must be ignored; Poundlock stamps its own.
        "recorded_at": "client-supplied-and-ignored"
    })
}

#[test]
fn capabilities_migrate_as_empty_on_an_existing_database_and_round_trip() {
    let (directory, store) = store();
    let (_endpoint, model, _) = configured(&store);
    assert!(model.capabilities.is_empty());
    // Simulate a database written before the additive capabilities column.
    drop(store);
    let path = directory.path().join("state.sqlite3");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE model_profiles DROP COLUMN capabilities_json")
        .unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&path, secrets).unwrap();
    assert!(
        store
            .model_profile(&model.id)
            .unwrap()
            .unwrap()
            .capabilities
            .is_empty()
    );
    let patched = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({
                "capabilities": [valid_capability("code_repair_verified", 8192, "model_default")]
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(patched.capabilities.len(), 1);
    let stored = &patched.capabilities[0];
    assert_eq!(stored.name, "code_repair_verified");
    assert_eq!(stored.status, "qualified");
    assert_eq!(stored.passed, 3);
    assert_eq!(stored.attempted, 3);
    // Poundlock stamps recorded_at itself rather than trusting the client.
    assert_ne!(stored.recorded_at, "client-supplied-and-ignored");
    assert!(!stored.recorded_at.is_empty());
    // Round-trips through storage unchanged.
    let reloaded = store.model_profile(&model.id).unwrap().unwrap();
    assert_eq!(reloaded.capabilities.len(), 1);
    assert_eq!(reloaded.capabilities[0].name, "code_repair_verified");
    assert_eq!(reloaded.capabilities[0].recorded_at, stored.recorded_at);
}

#[test]
fn capability_measurements_reject_unknown_names_and_duplicates() {
    let (_directory, store) = store();
    let (_endpoint, model, _) = configured(&store);
    let unknown = store.patch_model_profile(
        &model.id,
        &serde_json::from_value(json!({
            "capabilities": [valid_capability("not_a_real_capability", 8192, "model_default")]
        }))
        .unwrap(),
    );
    assert!(unknown.unwrap_err().to_string().contains("unknown"));

    let duplicate = store.patch_model_profile(
        &model.id,
        &serde_json::from_value(json!({
            "capabilities": [
                valid_capability("code_repair_verified", 8192, "model_default"),
                valid_capability("code_repair_verified", 8192, "model_default"),
            ]
        }))
        .unwrap(),
    );
    assert!(duplicate.unwrap_err().to_string().contains("one"));
}

#[test]
fn capability_measurements_enforce_pass_bounds_timestamp_and_toolkit_provenance() {
    let (_directory, store) = store();
    let (_endpoint, model, _) = configured(&store);

    let mut over_passed = valid_capability("code_repair_verified", 8192, "model_default");
    over_passed["passed"] = json!(5);
    over_passed["attempted"] = json!(3);
    assert!(
        store
            .patch_model_profile(
                &model.id,
                &serde_json::from_value(json!({"capabilities": [over_passed]})).unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("pass more attempts")
    );

    let mut bad_timestamp = valid_capability("code_repair_verified", 8192, "model_default");
    bad_timestamp["measured_at"] = json!("not-a-timestamp");
    assert!(
        store
            .patch_model_profile(
                &model.id,
                &serde_json::from_value(json!({"capabilities": [bad_timestamp]})).unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("RFC 3339")
    );

    let mut missing_run_ids = valid_capability("code_repair_verified", 8192, "model_default");
    missing_run_ids["run_ids"] = json!([]);
    assert!(
        store
            .patch_model_profile(
                &model.id,
                &serde_json::from_value(json!({"capabilities": [missing_run_ids]})).unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("poundlock_toolkit")
    );

    let mut missing_hash = valid_capability("code_repair_verified", 8192, "model_default");
    missing_hash["result_sha256"] = json!("");
    assert!(
        store
            .patch_model_profile(
                &model.id,
                &serde_json::from_value(json!({"capabilities": [missing_hash]})).unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("poundlock_toolkit")
    );

    // A non-toolkit source is not held to the run-id/hash requirement.
    let mut manual = valid_capability("code_repair_verified", 8192, "model_default");
    manual["source"] = json!("operator_note");
    manual["run_ids"] = json!([]);
    manual["result_sha256"] = json!("");
    assert_eq!(
        store
            .patch_model_profile(
                &model.id,
                &serde_json::from_value(json!({"capabilities": [manual]})).unwrap(),
            )
            .unwrap()
            .unwrap()
            .capabilities
            .len(),
        1
    );
}

#[test]
fn capabilities_are_dropped_when_context_tokens_or_thinking_change() {
    let (_directory, store) = store();
    let (_endpoint, model, _) = configured(&store);
    // configured() builds context_tokens: 8192, thinking: "model_default".
    let with_capability = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({
                "capabilities": [
                    valid_capability("code_repair_verified", 8192, "model_default"),
                    valid_capability("plan_tool_schema", 8192, "model_default"),
                ]
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(with_capability.capabilities.len(), 2);

    // An unrelated field patch leaves measurements untouched.
    let unrelated = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"name": "coding-renamed"})).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(unrelated.capabilities.len(), 2);

    // Changing context_tokens invalidates every measurement: it was measured
    // at a different configuration than the one now in effect.
    let context_changed = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"context_tokens": 16384})).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(context_changed.capabilities.is_empty());

    // Re-measure at the new context, then flip thinking: still invalidated.
    let with_capability = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({
                "capabilities": [valid_capability("code_repair_verified", 16384, "model_default")]
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(with_capability.capabilities.len(), 1);
    let thinking_changed = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"thinking": "off"})).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(thinking_changed.capabilities.is_empty());

    // A measurement taken with reasoning off survives a patch to off.
    let with_off_capability = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({
                "capabilities": [valid_capability("code_repair_verified", 16384, "off")]
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(with_off_capability.capabilities.len(), 1);
    let still_off = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"thinking": "off"})).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(still_off.capabilities.len(), 1);
}

#[test]
fn capabilities_do_not_survive_a_route_change() {
    // Evidence describes a model, not a profile name. Repointing the profile
    // at a different model or endpoint leaves the old model's measurements
    // published as `settings_match: exact` — routing would then send work to
    // an untested model on the strength of another one's score. The route
    // rotation already clears the label and the destination for this reason.
    let (_directory, store) = store();
    let (_endpoint, model, _) = configured(&store);
    let measured = |store: &Store| {
        store
            .patch_model_profile(
                &model.id,
                &serde_json::from_value(json!({
                    "capabilities": [valid_capability("code_repair_verified", 8192, "model_default")]
                }))
                .unwrap(),
            )
            .unwrap()
            .unwrap()
    };
    assert_eq!(measured(&store).capabilities.len(), 1);

    let model_swapped = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"model_id": "some-other-model"})).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert!(model_swapped.capabilities.is_empty());
    // The same rule the label and destination already follow.
    assert!(model_swapped.model_label.is_none());
    assert_eq!(model_swapped.data_destination, "unknown");

    // Re-pointing the endpoint moves the route of every profile on it.
    assert_eq!(measured(&store).capabilities.len(), 1);
    store
        .update_endpoint(
            &_endpoint.id,
            &serde_json::from_value(json!({
                "name": _endpoint.name,
                "kind": _endpoint.kind,
                "base_url": "http://127.0.0.1:9999/v1",
                "api_key": "k",
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
    let after_endpoint = store.model_profile(&model.id).unwrap().unwrap();
    assert!(after_endpoint.capabilities.is_empty());

    // A patch that re-points and re-measures in one go keeps what it supplied.
    let repointed_with_evidence = store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({
                "model_id": "third-model",
                "capabilities": [valid_capability("code_repair_verified", 8192, "model_default")]
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(repointed_with_evidence.capabilities.len(), 1);
}

#[test]
fn only_authoritative_not_found_answers_count_toward_retirement() {
    use poundlock::store::retirement::model_gone;
    for (status, body) in [
        (429, "rate limited: model not found in quota"),
        (500, "model not found"),
        (503, "no endpoints found"),
        (504, ""),
        (404, "<html>Not Found</html>"),
        (400, "max_tokens is too large"),
    ] {
        assert_eq!(model_gone(status, body), None, "{status} {body}");
    }
    assert!(
        model_gone(
            404,
            r#"{"error":{"message":"No endpoints found for openai/gpt-5.6-sol."}}"#
        )
        .is_some()
    );
    assert!(
        model_gone(
            400,
            r#"{"error":{"message":"Invalid model name passed in model=gone"}}"#
        )
        .is_some()
    );
    assert!(model_gone(400, "openai/x is not a valid model ID").is_some());
}

#[test]
fn a_profile_retires_after_three_hard_failures_spanning_thirty_minutes() {
    use poundlock::store::retirement::RouteOutcome;
    let (_directory, store) = store();
    let (_, model, _) = configured(&store);
    let gone = RouteOutcome::Gone {
        status: 404,
        detail: "HTTP 404: upstream reported \"no endpoints found\"".into(),
    };
    let record = |outcome: &RouteOutcome, at: &str| {
        store
            .record_route_outcome_at(&model.id, "relay", outcome, at.into())
            .unwrap()
    };

    // Three quick failures are not enough: the span is under thirty minutes.
    assert!(!record(&gone, "2026-09-23T10:00:00Z"));
    assert!(!record(&gone, "2026-09-23T10:05:00Z"));
    assert!(!record(&gone, "2026-09-23T10:10:00Z"));
    // Any success resets the count.
    assert!(!record(&RouteOutcome::Served, "2026-09-23T10:20:00Z"));
    assert!(
        store
            .profile_retirement(&model.id)
            .unwrap()
            .unwrap()
            .hard_failures
            .is_empty()
    );

    assert!(!record(&gone, "2026-09-23T11:00:00Z"));
    assert!(!record(&gone, "2026-09-23T11:15:00Z"));
    assert!(
        record(&gone, "2026-09-23T11:30:00Z"),
        "third failure, thirty minutes after the first"
    );
    let retirement = store.profile_retirement(&model.id).unwrap().unwrap();
    let disabled = retirement.disabled.expect("disabled");
    assert_eq!(disabled.source, "relay");
    assert_eq!(disabled.evidence.len(), 3);
    assert!(disabled.reason.contains("no endpoints found"));

    // Success afterwards keeps it disabled; only an operator re-enables.
    record(&RouteOutcome::Served, "2026-09-23T12:00:00Z");
    assert!(
        store
            .profile_retirement(&model.id)
            .unwrap()
            .unwrap()
            .disabled
            .is_some()
    );
    assert!(store.enable_profile(&model.id).unwrap());
    let retirement = store.profile_retirement(&model.id).unwrap().unwrap();
    assert!(retirement.disabled.is_none() && retirement.hard_failures.is_empty());
    // The profile itself, and its configuration, were never removed.
    assert_eq!(
        store.model_profile(&model.id).unwrap().unwrap().name,
        "coding"
    );
}

#[test]
fn a_price_refresh_keeps_the_route_revision_but_a_new_served_model_moves_it() {
    let (_directory, store) = store();
    let (_, model, _) = configured(&store);
    let patch = |catalog: serde_json::Value| {
        let patch: PatchModelProfile = serde_json::from_value(json!({"catalog": catalog})).unwrap();
        store
            .patch_model_profile(&model.id, &patch)
            .unwrap()
            .unwrap()
    };
    let priced = |input: f64| {
        json!({"served_model": "openrouter/openai/gpt-6-sol",
        "pricing": {"input_per_mtok": input, "output_per_mtok": 10.0, "source": "litellm_route",
                    "refreshed_at": "2026-09-23T19:00:00Z"}})
    };
    let first = store.routing_revision(&patch(priced(2.0))).unwrap();
    let repriced = store.routing_revision(&patch(priced(3.0))).unwrap();
    assert_eq!(first, repriced, "a price is not a different route");
    let moved = store
        .routing_revision(&patch(
            json!({"served_model": "openrouter/openai/gpt-7-sol"}),
        ))
        .unwrap();
    assert_ne!(repriced, moved, "a different model behind the name is");
}

#[test]
fn a_default_thinking_budget_is_a_bounded_llamacpp_setting() {
    let (directory, store) = store();
    let (_, model, _) = configured(&store);
    assert_eq!(model.thinking_budget_tokens, None);
    let node = store
        .enroll_node(&EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "orion".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap()
        .node_id;
    let patch = |body: serde_json::Value| {
        store.patch_model_profile(&model.id, &serde_json::from_value(body).unwrap())
    };
    // Nothing else reads the field, so a profile that is not on llamacpp
    // would carry a setting that never applies.
    let refused = patch(json!({"thinking_budget_tokens": 1024})).unwrap_err();
    assert!(refused.to_string().contains("llamacpp"), "{refused}");
    let set = patch(json!({
        "node_id": node, "runtime_profile": "llamacpp", "thinking_budget_tokens": 2048
    }))
    .unwrap()
    .unwrap();
    assert_eq!(set.thinking_budget_tokens, Some(2048));
    // Omitted leaves it alone; it survives a reload.
    let other = patch(json!({"idle_seconds": null})).unwrap().unwrap();
    assert_eq!(other.thinking_budget_tokens, Some(2048));
    assert_eq!(
        store
            .model_profile(&model.id)
            .unwrap()
            .unwrap()
            .thinking_budget_tokens,
        Some(2048)
    );
    // Whole numbers from one to the output limit (4096 here), and the limit
    // cannot be lowered underneath it.
    for bad in [json!(0), json!(4097)] {
        let refused = patch(json!({"thinking_budget_tokens": bad})).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("between 1 and max_output_tokens"),
            "{refused}"
        );
    }
    let refused = patch(json!({"max_output_tokens": 1024})).unwrap_err();
    assert!(
        refused.to_string().contains("max_output_tokens"),
        "{refused}"
    );
    assert!(patch(json!({"max_output_tokens": 2048})).is_ok());
    // Leaving llamacpp while it is set is refused, and null clears it.
    let refused = patch(json!({"node_id": "", "runtime_profile": ""})).unwrap_err();
    assert!(refused.to_string().contains("llamacpp"), "{refused}");
    let cleared = patch(json!({"node_id": "", "runtime_profile": "",
        "thinking_budget_tokens": null}))
    .unwrap()
    .unwrap();
    assert_eq!(cleared.thinking_budget_tokens, None);
    assert_eq!(cleared.runtime_profile, None);

    // A database written before the column reads as no default.
    patch(json!({"node_id": node, "runtime_profile": "llamacpp",
        "thinking_budget_tokens": 512}))
    .unwrap();
    drop(store);
    let path = directory.path().join("state.sqlite3");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE model_profiles DROP COLUMN thinking_budget_tokens")
        .unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let reopened = Store::open(&path, secrets).unwrap();
    assert_eq!(
        reopened
            .model_profile(&model.id)
            .unwrap()
            .unwrap()
            .thinking_budget_tokens,
        None
    );
}

#[test]
fn revoking_a_token_cancels_its_unfinished_jobs_and_ends_their_inference() {
    let (directory, store) = store();
    let (_, model, project) = configured(&store);
    let token = |name: &str| {
        store
            .create_api_token(&CreateApiToken {
                name: name.into(),
                permissions: vec!["jobs:submit".into()],
            })
            .unwrap()
            .details
            .id
    };
    let (leaked, other) = (token("leaked"), token("other"));
    let submit = |key: &str, token: Option<&str>| {
        store
            .submit_job_for(
                &SubmitJob {
                    source: None,
                    idempotency_key: key.into(),
                    policy: Default::default(),
                    project_id: project.id.clone(),
                    model_profile_id: model.id.clone(),
                    harness_profile_id: "pi".into(),
                    prompt: "change it".into(),
                },
                token,
                10,
            )
            .unwrap()
    };
    let queued = submit("queued", Some(&leaked));
    let running = submit("running", Some(&leaked));
    let bystander = submit("bystander", Some(&other));
    let operator = submit("operator", None);
    store
        .set_job_status(&running.id, "ready", "ready", "Ready")
        .unwrap();
    assert_eq!(
        store.claim_runner_job(None).unwrap(),
        Some(running.id.clone())
    );
    let inference = store.issue_job_inference_token(&running.id).unwrap();
    assert!(
        store
            .job_inference_authorized(&running.id, &inference)
            .unwrap()
    );

    assert!(store.revoke_api_token(&leaked).unwrap());
    for job in [&queued, &running] {
        let job = store.job(&job.id).unwrap().unwrap();
        assert!(job.cancel_requested, "{} kept running", job.id);
        assert!(
            store
                .events(&job.id, 0)
                .unwrap()
                .iter()
                .any(|event| event.kind == "cancel_requested" && event.message.contains("revoked"))
        );
    }
    for job in [&bystander, &operator] {
        assert!(!store.job(&job.id).unwrap().unwrap().cancel_requested);
    }
    assert!(
        !store
            .job_inference_authorized(&running.id, &inference)
            .unwrap()
    );

    // The token check stands on its own: a job whose token was revoked before
    // revocation cancelled jobs (an older database) still loses inference.
    store
        .set_job_status(&bystander.id, "ready", "ready", "Ready")
        .unwrap();
    assert_eq!(
        store.claim_runner_job(None).unwrap(),
        Some(bystander.id.clone())
    );
    let inference = store.issue_job_inference_token(&bystander.id).unwrap();
    rusqlite::Connection::open(directory.path().join("state.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE api_tokens SET revoked_at='2026-01-01T00:00:00Z' WHERE id=?1",
            [&other],
        )
        .unwrap();
    assert!(
        !store
            .job_inference_authorized(&bystander.id, &inference)
            .unwrap()
    );
    assert!(store.issue_job_inference_token(&bystander.id).is_err());
}
