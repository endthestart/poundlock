use super::*;
#[test]
fn policy_rejects_escaping_paths_unknown_fields_and_unbounded_repair() {
    for path in ["../tests", "/tests", "a/.git/config", "a\\b", "a\0b"] {
        assert!(relative_path(path).is_err(), "{path}");
    }
    for path in ["", ".", "tests", "one/tests/check.py"] {
        relative_path(path).unwrap();
    }
    assert!(
        serde_json::from_value::<JobPolicy>(serde_json::json!({"netwrok":"outbound"})).is_err()
    );
    let mut policy = JobPolicy {
        max_repairs: 1,
        ..Default::default()
    };
    assert!(policy.validate().is_err());
    policy.verification.commands.push(VerificationCommand {
        command: "node --test".into(),
        cwd: "".into(),
        timeout_seconds: 300,
    });
    policy.validate().unwrap();
    policy.max_repairs = 4;
    assert!(policy.validate().is_err());
}

#[test]
fn dependency_freshness_defaults_false_and_round_trips() {
    assert!(!JobPolicy::default().dependency_freshness);
    let policy: JobPolicy = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(!policy.dependency_freshness);
    let policy: JobPolicy =
        serde_json::from_value(serde_json::json!({"dependency_freshness": true})).unwrap();
    assert!(policy.dependency_freshness);
}

#[test]
fn protected_path_accepts_bare_strings_and_explicit_modes() {
    let bare: ProtectedPath = serde_json::from_value(serde_json::json!("tests")).unwrap();
    assert_eq!(bare.path(), "tests");
    assert_eq!(bare.mode(), ProtectionMode::Strict);
    assert_eq!(
        serde_json::to_value(&bare).unwrap(),
        serde_json::json!("tests")
    );

    let strict_entry: ProtectedPath =
        serde_json::from_value(serde_json::json!({"path": "tests"})).unwrap();
    assert_eq!(strict_entry.mode(), ProtectionMode::Strict);

    let lenient: ProtectedPath = serde_json::from_value(
        serde_json::json!({"path": "evaluation/bootstrap", "mode": "existing_only"}),
    )
    .unwrap();
    assert_eq!(lenient.path(), "evaluation/bootstrap");
    assert_eq!(lenient.mode(), ProtectionMode::ExistingOnly);
}

#[test]
fn existing_only_mode_ignores_additions_but_still_catches_modification_and_deletion() {
    let mut original = BTreeMap::new();
    original.insert(PathBuf::from("a"), "file:false:hash-a".to_owned());
    original.insert(PathBuf::from("b"), "file:false:hash-b".to_owned());

    // An addition alone is not a change under ExistingOnly.
    let mut with_addition = original.clone();
    with_addition.insert(PathBuf::from("venv"), "directory".to_owned());
    assert!(!protected_path_changed(
        ProtectionMode::ExistingOnly,
        &original,
        &with_addition
    ));
    // The same tree still fails Strict, which is exactly today's behaviour.
    assert!(protected_path_changed(
        ProtectionMode::Strict,
        &original,
        &with_addition
    ));

    // Modifying an existing entry still fails under both modes.
    let mut modified = original.clone();
    modified.insert(PathBuf::from("a"), "file:false:hash-a-changed".to_owned());
    assert!(protected_path_changed(
        ProtectionMode::ExistingOnly,
        &original,
        &modified
    ));

    // Deleting an existing entry still fails under both modes.
    let mut deleted = original.clone();
    deleted.remove(&PathBuf::from("b"));
    assert!(protected_path_changed(
        ProtectionMode::ExistingOnly,
        &original,
        &deleted
    ));

    // No change at all is never flagged.
    assert!(!protected_path_changed(
        ProtectionMode::ExistingOnly,
        &original,
        &original
    ));
}

#[test]
fn effective_budgets_divide_evenly_across_attempts_and_never_duplicate_the_enforced_value() {
    assert_eq!(effective_attempts(0), 1);
    assert_eq!(effective_attempts(3), 4);
    assert_eq!(effective_output_bytes_per_stream(0), 2 * 1024 * 1024);
    assert_eq!(effective_output_bytes_per_stream(1), 1024 * 1024);
    assert_eq!(effective_output_bytes_per_stream(3), 2 * 1024 * 1024 / 4);
    assert_eq!(effective_patch_bytes(0), 10 * 1024 * 1024);
    assert_eq!(effective_patch_bytes(3), 10 * 1024 * 1024 / 4);
}
