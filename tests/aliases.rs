use poundlock::crypto::SecretBox;
use poundlock::domain::*;
use poundlock::store::Store;

fn store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
    (directory, store)
}

fn endpoint(store: &Store) -> Endpoint {
    store
        .create_endpoint(&CreateEndpoint {
            name: "endpoint".into(),
            kind: "openai".into(),
            base_url: "http://localhost:8080/v1".into(),
            api_key: "k".into(),
            managed_identity: Default::default(),
        })
        .unwrap()
}

#[test]
fn aliases_resolve_to_the_same_profile_and_cannot_collide() {
    let (_directory, store) = store();
    let endpoint = endpoint(&store);
    let profile = store
        .create_model_profile(&CreateModelProfile {
            name: "workstation-general".into(),
            aliases: vec!["coding".into()],
            endpoint_id: endpoint.id.clone(),
            model_id: "workstation-active".into(),
            model_label: None,
            context_tokens: 131072,
            max_output_tokens: 8192,
            uses: None,
            workload_class: Some("interactive-coding".into()),
            wake_policy: "on-demand".into(),
            idle_seconds: None,
            data_destination: "unknown".into(),
            thinking: "model_default".into(),
            thinking_budget_tokens: None,
            capabilities: Vec::new(),
            node_id: None,
            runtime_profile: None,
        })
        .unwrap();
    assert_eq!(
        store.model_profile_by_name("coding").unwrap().unwrap().id,
        profile.id
    );
    store
        .create_model_profile(&CreateModelProfile {
            name: "other".into(),
            aliases: vec!["coding".into()],
            endpoint_id: endpoint.id,
            model_id: "x".into(),
            model_label: None,
            context_tokens: 8192,
            max_output_tokens: 1024,
            uses: None,
            workload_class: Some("interactive-coding".into()),
            wake_policy: "never".into(),
            idle_seconds: None,
            data_destination: "unknown".into(),
            thinking: "model_default".into(),
            thinking_budget_tokens: None,
            capabilities: Vec::new(),
            node_id: None,
            runtime_profile: None,
        })
        .unwrap_err();
}

#[test]
fn instruction_files_round_trip_on_projects() {
    let (_directory, store) = store();
    let project = store
        .create_project(&CreateProject {
            name: "anderpunk".into(),
            roots: vec!["anderpunk".into()],
            instruction_files: vec!["AGENTS.md".into(), "docs/tutorial/README.md".into()],
        })
        .unwrap();
    assert_eq!(
        store
            .project(&project.id)
            .unwrap()
            .unwrap()
            .instruction_files,
        vec!["AGENTS.md", "docs/tutorial/README.md"]
    );
    store
        .patch_project(
            &project.id,
            &PatchProject {
                instruction_files: Some(vec!["AGENTS.md".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        store
            .project(&project.id)
            .unwrap()
            .unwrap()
            .instruction_files,
        vec!["AGENTS.md"]
    );
}
