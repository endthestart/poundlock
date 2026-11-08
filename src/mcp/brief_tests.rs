use super::*;

fn brief() -> TaskBrief {
    TaskBrief {
        task: "Add a --verbose flag to the CLI.".into(),
        files: vec!["src/cli.rs".into()],
        context: Some("Reuse the existing Args struct.".into()),
        acceptance_checks: vec!["cargo test".into()],
        frozen_paths: vec!["tests/cli.rs".into()],
        protect_existing: vec!["fixtures/".into()],
        ..Default::default()
    }
}

#[test]
fn checks_and_protections_become_enforced_policy() {
    let policy = brief().policy();
    assert_eq!(policy.verification.commands[0].command, "cargo test");
    assert_eq!(policy.verification.commands[0].timeout_seconds, 300);
    assert_eq!(
        policy.verification.protected_paths,
        vec![
            ProtectedPath::Bare("tests/cli.rs".into()),
            ProtectedPath::Entry {
                path: "fixtures".into(),
                mode: ProtectionMode::ExistingOnly,
            },
        ]
    );
    assert_eq!(policy.max_repairs, 1);
    assert_eq!(policy.network, JobNetwork::InferenceOnly);
    assert!(brief().validate().is_ok());
}

#[test]
fn a_brief_without_checks_gets_no_repairs() {
    let plain = TaskBrief {
        task: "Summarise the README.".into(),
        ..Default::default()
    };
    assert_eq!(plain.policy().max_repairs, 0);
    assert!(plain.validate().is_ok());
    let repairs_without_checks = TaskBrief {
        max_repairs: Some(2),
        ..plain
    };
    assert!(repairs_without_checks.validate().is_err());
}

#[test]
fn the_prompt_restates_files_context_checks_and_frozen_paths() {
    let prompt = brief().prompt();
    assert!(prompt.starts_with("Add a --verbose flag to the CLI."));
    assert!(prompt.contains("- src/cli.rs"));
    assert!(prompt.contains("Reuse the existing Args struct."));
    assert!(prompt.contains("- `cargo test`"));
    assert!(prompt.contains("Never weaken, skip or delete a test"));
    assert!(prompt.contains("- tests/cli.rs\n- fixtures/"));
    assert!(prompt.ends_with("anything you could not do."));
}

#[test]
fn a_budget_becomes_the_jobs_own_limits() {
    let brief = TaskBrief {
        task: "x".into(),
        budget_minutes: Some(20),
        budget_usd: Some(0.5),
        ..Default::default()
    };
    brief.validate().unwrap();
    assert_eq!(
        brief.policy().budget,
        crate::job_budget::JobBudget {
            wall_seconds: Some(1200),
            frontier_cents: Some(50)
        }
    );
    assert!(TaskBrief::default().policy().budget.is_empty());
}

#[test]
fn bad_briefs_are_refused() {
    for bad in [
        TaskBrief::default(),
        TaskBrief {
            task: "x".into(),
            files: vec!["../outside.rs".into()],
            ..Default::default()
        },
        TaskBrief {
            task: "x".into(),
            frozen_paths: vec!["/etc/passwd".into()],
            ..Default::default()
        },
        TaskBrief {
            task: "x".into(),
            network: Some("anywhere".into()),
            ..Default::default()
        },
        TaskBrief {
            task: "x".into(),
            budget_minutes: Some(0),
            ..Default::default()
        },
        TaskBrief {
            task: "x".into(),
            budget_usd: Some(f64::NAN),
            ..Default::default()
        },
        TaskBrief {
            task: "x".into(),
            acceptance_checks: vec!["make".into()],
            check_timeout_seconds: Some(10_000),
            ..Default::default()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
}
