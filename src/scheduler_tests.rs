use super::*;
use crate::domain::*;
use crate::server::capacity::Occupancy;
use crate::store::Store;
use serde_json::json;
use std::collections::BTreeSet;

fn store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let secrets =
        crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    (directory, store)
}

fn enroll(store: &Store) -> String {
    let token = store.create_enrollment_token(15, None).unwrap();
    store
        .enroll_node(&EnrollNode {
            token,
            name: "large-node".into(),
            platform: "linux".into(),
            capabilities: json!({"runtimes":["large-runtime"]}),
        })
        .unwrap()
        .node_id
}

#[test]
fn interactive_occupancy_renews_until_drop() {
    let (_directory, store) = store();
    let node_id = enroll(&store);
    let occupancy = Occupancy::new();
    let slot = Occupancy::exclusive_slot(&ModelProfile {
        id: "profile".into(),
        name: "coding-large".into(),
        aliases: Vec::new(),
        endpoint_id: "endpoint".into(),
        model_id: "upstream-large".into(),
        model_label: None,
        context_tokens: 131072,
        max_output_tokens: 8192,
        uses: BTreeSet::from(["interactive".to_string()]),
        workload_class: "interactive-coding".into(),
        wake_policy: "on-demand".into(),
        node_id: Some(node_id.clone()),
        runtime_profile: Some("large-runtime".into()),
        idle_seconds: None,
        data_destination: "unknown".into(),

        data_destination_source: "profile".into(),
        thinking: "model_default".into(),
        thinking_budget_tokens: None,
        capabilities: Vec::new(),
        catalog: Default::default(),
        retirement: Default::default(),
        created_at: "now".into(),
        last_check: ResourceCheck::default(),
    })
    .unwrap();
    let mut renewals = HashMap::new();
    {
        let _guard = occupancy
            .try_claim_interactive(slot.clone(), "interactive-lease")
            .unwrap();
        let held = occupancy.interactive_leases();
        enqueue_interactive_renewals(&store, &held, &mut renewals).unwrap();
        enqueue_interactive_renewals(&store, &held, &mut renewals).unwrap();
        let renews: Vec<_> = store
            .list_node_actions(&node_id)
            .unwrap()
            .into_iter()
            .filter(|action| action.kind == "renew")
            .collect();
        assert_eq!(renews.len(), 1);
        assert_eq!(renews[0].payload["lease_id"], "interactive-lease");
        assert_eq!(renews[0].payload["runtime_profile"], "large-runtime");
    }
    renewals.retain(|id, _| {
        occupancy
            .interactive_leases()
            .iter()
            .any(|lease| &lease.lease_id == id)
    });
    let held = occupancy.interactive_leases();
    assert!(held.is_empty());
    enqueue_interactive_renewals(&store, &held, &mut renewals).unwrap();
    assert_eq!(
        store
            .list_node_actions(&node_id)
            .unwrap()
            .iter()
            .filter(|action| action.kind == "renew")
            .count(),
        1
    );
    assert!(renewals.is_empty());
}

#[test]
fn completion_and_unplaced_profiles_do_not_enqueue_interactive_renew() {
    let (_directory, store) = store();
    let node_id = enroll(&store);
    let occupancy = Occupancy::new();
    assert!(
        Occupancy::exclusive_slot(&ModelProfile {
            id: "prediction".into(),
            name: "predict".into(),
            aliases: Vec::new(),
            endpoint_id: "endpoint".into(),
            model_id: "upstream-predict".into(),
            model_label: None,
            context_tokens: 4096,
            max_output_tokens: 128,
            uses: BTreeSet::from(["completion".to_string()]),
            workload_class: "fast-code-completion".into(),
            wake_policy: "never".into(),
            node_id: Some(node_id.clone()),
            runtime_profile: Some("prediction".into()),
            idle_seconds: None,
            data_destination: "unknown".into(),

            data_destination_source: "profile".into(),
            thinking: "model_default".into(),
            thinking_budget_tokens: None,
            capabilities: Vec::new(),
            catalog: Default::default(),
            retirement: Default::default(),
            created_at: "now".into(),
            last_check: ResourceCheck::default(),
        })
        .is_none()
    );
    let mut renewals = HashMap::new();
    enqueue_interactive_renewals(&store, &occupancy.interactive_leases(), &mut renewals).unwrap();
    assert!(store.list_node_actions(&node_id).unwrap().is_empty());
}

#[test]
fn durable_job_renew_still_uses_the_job_attempt_lease() {
    let (_directory, store) = store();
    let node_id = enroll(&store);
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
            name: "coding-durable".into(),
            aliases: Vec::new(),
            endpoint_id: endpoint.id,
            model_id: "upstream-large".into(),
            model_label: None,
            context_tokens: 131072,
            max_output_tokens: 8192,
            uses: None,
            workload_class: Some("durable-coding-agent".into()),
            wake_policy: "on-demand".into(),
            node_id: Some(node_id.clone()),
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
            roots: vec!["repo".into()],
            instruction_files: Vec::new(),
        })
        .unwrap();
    let job = store
        .submit_job(&SubmitJob {
            source: None,
            idempotency_key: "durable".into(),
            policy: Default::default(),
            project_id: project.id,
            model_profile_id: model.id,
            harness_profile_id: "pi".into(),
            prompt: "run".into(),
        })
        .unwrap();
    store
        .enqueue_action(
            &node_id,
            Some(&job.id),
            "renew",
            json!({
                "runtime_profile": "large-runtime",
                "lease_id": job.attempt_id,
                "operation_id": "op"
            }),
        )
        .unwrap();
    let occupancy = Occupancy::new();
    let mut renewals = HashMap::new();
    enqueue_interactive_renewals(&store, &occupancy.interactive_leases(), &mut renewals).unwrap();
    let renews = store.list_node_actions(&node_id).unwrap();
    assert_eq!(renews.len(), 1);
    assert_eq!(renews[0].job_id.as_deref(), Some(job.id.as_str()));
    assert_eq!(renews[0].payload["lease_id"], job.attempt_id);
}
