use super::*;
use axum::{Json, Router, routing::post};

async fn controller(app: Router) -> (RunnerClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        RunnerClient::new(format!("http://{address}"), "test-token".into()).unwrap(),
        server,
    )
}

#[tokio::test]
async fn a_cancelled_lease_stops_work_at_once() {
    let (client, server) = controller(Router::new().route(
        "/runner/v1/jobs/test/heartbeat",
        post(|| async { Json(json!({"cancel_requested":true})) }),
    ))
    .await;
    let error = monitor(
        &client,
        "test",
        tokio::time::Instant::now() + Duration::from_secs(5),
        std::future::pending::<Result<()>>(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Stopped>().unwrap().code,
        "cancelled_or_lease_lost"
    );
    server.abort();
}

/// A failed heartbeat says nothing about whether this job is still ours: the
/// lease lasts 120 s and one request failing is not the lease ending. Giving
/// up on the first failure meant a ten-second controller restart killed every
/// job in flight.
#[tokio::test(start_paused = true)]
async fn a_brief_controller_outage_does_not_kill_a_running_attempt() {
    // No routes at all: every heartbeat is a 404, which the client reports as
    // an error — the same shape as a restarting controller.
    let (client, server) = controller(Router::new()).await;
    let running = monitor(
        &client,
        "test",
        tokio::time::Instant::now() + Duration::from_secs(3600),
        std::future::pending::<Result<()>>(),
    );
    tokio::pin!(running);

    // Well inside the lease, the attempt is still running. This is the whole
    // regression: it used to stop on the very first failed heartbeat.
    let early = tokio::time::timeout(LEASE_GRACE - Duration::from_secs(20), &mut running).await;
    assert!(early.is_err(), "a brief outage must not stop the attempt");

    // And the grace still ends before the controller's 120 s lease does, so
    // two runners never hold one job.
    assert!(LEASE_GRACE < Duration::from_secs(120));
    server.abort();
}

#[tokio::test]
async fn stalled_heartbeat_cannot_extend_the_worker_deadline() {
    let (client, server) = controller(Router::new().route(
        "/runner/v1/jobs/test/heartbeat",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Json(json!({"cancel_requested":false}))
        }),
    ))
    .await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        monitor(
            &client,
            "test",
            tokio::time::Instant::now() + Duration::from_millis(100),
            std::future::pending::<Result<()>>(),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Stopped>().unwrap().code,
        "wall_budget_exhausted"
    );
    server.abort();
}

#[tokio::test]
async fn result_survives_failed_delivery_and_is_removed_only_after_acknowledgement() {
    let directory = tempfile::tempdir().unwrap();
    let path = Report {
        job_id: "test".into(),
        cancelled: false,
        error: None,
        result: Some(RunnerResult {
            patches: vec![],
            output: "retained output".into(),
            provenance: json!({}),
            cleanup: json!({}),
        }),
    }
    .save(directory.path())
    .unwrap();
    let (unavailable, server) = controller(Router::new()).await;
    delivery::deliver_pending(&unavailable, directory.path(), &HashSet::new())
        .await
        .unwrap();
    assert!(path.exists());
    server.abort();
    let (available, server) = controller(Router::new().route(
        "/runner/v1/jobs/test/complete",
        post(|Json(result): Json<RunnerResult>| async move {
            assert_eq!(result.output, "retained output");
            Json(json!({"accepted":true}))
        }),
    ))
    .await;
    delivery::deliver_pending(
        &available,
        directory.path(),
        &HashSet::from(["test".into()]),
    )
    .await
    .unwrap();
    assert!(
        path.exists(),
        "an active worker's provisional report was delivered"
    );
    delivery::deliver_pending(&available, directory.path(), &HashSet::new())
        .await
        .unwrap();
    assert!(!path.exists());
    server.abort();
}

#[test]
fn cleanup_retry_preserves_cancelled_evidence_and_records_confirmed_removal() {
    let directory = tempfile::tempdir().unwrap();
    let mut report = Report {
        job_id: "cancelled-job".into(),
        cancelled: true,
        error: Some(json!({"code":"cleanup_interrupted"})),
        result: Some(RunnerResult {
            patches: vec![],
            output: "partial output".into(),
            provenance: json!({"verification":{"status":"pending"}}),
            cleanup: json!({"worktrees_removed":false,"attempt_directory_removed":false}),
        }),
    };
    let path = report.save(directory.path()).unwrap();
    let retained: Report = serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(
        retained.result.unwrap().cleanup["attempt_directory_removed"],
        false
    );
    report.confirm_recovered_cleanup(
        json!({"code":"attempt_failed","message":"worker removal failed: 409"}),
        false,
    );
    report.save(directory.path()).unwrap();
    let retained: Report = serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    assert!(retained.cancelled);
    assert_eq!(retained.error.unwrap()["code"], "attempt_failed");
    let result = retained.result.unwrap();
    assert_eq!(result.output, "partial output");
    assert_eq!(result.provenance["verification"]["status"], "unverified");
    for field in [
        "containers_removed",
        "worktrees_removed",
        "attempt_directory_removed",
    ] {
        assert_eq!(result.cleanup[field], true);
    }
    assert_eq!(result.cleanup["recovered_after_restart"], false);
}

#[test]
fn restart_confirms_cleanup_without_reexecuting_or_losing_candidate() {
    let results = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let attempt = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir(results.path().join("active")).unwrap();
    std::fs::write(
        results
            .path()
            .join("active")
            .join(format!("{attempt}.json")),
        serde_json::to_vec(&json!({"job_id":id,"attempt_id":attempt})).unwrap(),
    )
    .unwrap();
    std::fs::create_dir(work.path().join(&attempt)).unwrap();
    std::fs::write(work.path().join(&attempt).join("partial.txt"), "candidate").unwrap();
    let path = Report {
        job_id: id,
        cancelled: false,
        error: Some(json!({"code":"cleanup_interrupted"})),
        result: Some(RunnerResult {
            patches: vec![],
            output: "retained candidate".into(),
            provenance: json!({"verification":{"status":"pending"}}),
            cleanup: json!({}),
        }),
    }
    .save(results.path())
    .unwrap();
    delivery::recover(results.path(), work.path()).unwrap();
    assert!(!work.path().join(attempt).exists());
    assert_eq!(
        std::fs::read_dir(results.path().join("active"))
            .unwrap()
            .count(),
        0
    );
    let retained: Report = serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    assert_eq!(retained.error.unwrap()["code"], "runner_interrupted");
    let result = retained.result.unwrap();
    assert_eq!(result.output, "retained candidate");
    assert_eq!(result.cleanup["recovered_after_restart"], true);
    assert_eq!(result.cleanup["attempt_directory_removed"], true);
}

#[test]
fn confirmed_retry_updates_filesystem_failure_without_changing_verification() {
    let mut report = Report {
        job_id: "failed-cleanup".into(),
        cancelled: false,
        error: Some(json!({"code":"cleanup_failed"})),
        result: Some(RunnerResult {
            patches: vec![],
            output: "candidate".into(),
            provenance: json!({"verification":{"status":"passed"}}),
            cleanup: json!({"containers_removed":true,"attempt_directory_removed":false}),
        }),
    };
    report.confirm_recovered_cleanup(json!({"code":"attempt_failed"}), false);
    assert_eq!(report.error.as_ref().unwrap()["code"], "attempt_failed");
    let result = report.result.as_ref().unwrap();
    assert_eq!(result.cleanup["attempt_directory_removed"], true);
    assert_eq!(result.provenance["verification"]["status"], "passed");
    // A later recovery must not overwrite an already finalized failure report.
    report.confirm_recovered_cleanup(json!({"code":"runner_interrupted"}), true);
    assert_eq!(report.error.unwrap()["code"], "attempt_failed");
}

#[tokio::test]
async fn claim_reports_the_runners_configured_wall_seconds() {
    let (client, server) = controller(Router::new().route(
        "/runner/v1/claim",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(body["wall_seconds"], 1800);
            Json(Value::Null)
        }),
    ))
    .await;
    assert!(client.claim(1800).await.unwrap().is_none());
    server.abort();
}

// B2: a wall-deadline cutoff must be reported as `wall_budget_after_turn`
// when the agent's own final turn had already settled, and
// `wall_budget_mid_turn` otherwise — mirroring `output_limit_after_turn`/
// `output_limit_mid_turn` (`harness::agent_turn_settled`'s own tests cover
// what "settled" means; this covers the mapping to a code and message).
#[test]
fn wall_budget_code_distinguishes_a_settled_turn_from_a_mid_turn_cutoff() {
    let (settled_code, settled_message) = wall_budget_outcome(true);
    assert_eq!(settled_code, "wall_budget_after_turn");
    assert!(settled_message.contains("finished"));
    let (mid_code, mid_message) = wall_budget_outcome(false);
    assert_eq!(mid_code, "wall_budget_mid_turn");
    assert!(mid_message.contains("working"));
    assert_ne!(settled_code, mid_code);
}

// B2: a wall-budget cutoff runs verification only when the agent's turn had
// settled — the deadline has already passed, so the bounded post-deadline
// window must not open for work that was still in flight. An output cutoff is
// the opposite case and is covered by
// `an_output_cutoff_still_checks_the_patch_it_kept`.
#[test]
fn verification_only_runs_for_settled_cutoffs_not_unrelated_failures() {
    use sandbox::{verification_worth_running, wall_budget_failure};
    let wall_budget = || -> anyhow::Error {
        Stopped {
            code: "wall_budget_exhausted",
            message: "exhausted",
            cancelled: false,
        }
        .into()
    };
    let cancelled = || -> anyhow::Error {
        Stopped {
            code: "cancelled_or_lease_lost",
            message: "cancelled",
            cancelled: true,
        }
        .into()
    };
    // Wall-budget cutoff: only a settled turn allows it.
    let stopped = wall_budget();
    assert!(wall_budget_failure(Some(&stopped)));
    assert!(verification_worth_running(Some(&stopped), true));
    assert!(!verification_worth_running(Some(&stopped), false));
    // Any other failure never runs verification, settled or not.
    let other = cancelled();
    assert!(!wall_budget_failure(Some(&other)));
    assert!(!verification_worth_running(Some(&other), true));
}

/// An output cutoff keeps a patch, and that patch is worth checking whether or
/// not the agent got to finish speaking.
///
/// `turn_settled` cannot answer that question here. The capture is head-only
/// (`runner_docker::frames`), so when the agent's own stdout overflows, the
/// terminal `message_end` — the evidence that the turn settled — is exactly
/// what was discarded. Gating on it denied verification permanently to the
/// jobs that most needed it: a consumer measured 22 of 24 failed jobs handing
/// back a usable patch, and had to re-run the checks locally against work that
/// was already done.
///
/// The wall clock still bounds this path, so the checks cost seconds against
/// an hour already spent.
#[test]
fn an_output_cutoff_still_checks_the_patch_it_kept() {
    use sandbox::verification_worth_running;
    for turn_settled in [true, false] {
        assert!(
            verification_worth_running(None, turn_settled),
            "an output cutoff must check its patch (settled={turn_settled})"
        );
    }
}

// B3: a patch-budget failure should name its largest contributors instead
// of just a total size, so "the agent polluted the tree" is visible without
// inferring it from a byte count alone.
#[test]
fn patch_budget_diagnostic_names_its_largest_contributors() {
    use sandbox::{describe_largest_contributors, largest_diff_contributors};
    let diff = b"diff --git a/small.txt b/small.txt\nindex 0000000..1111111 100644\n--- a/small.txt\n+++ b/small.txt\n@@ -0,0 +1 @@\n+hi\ndiff --git a/vendor/big.bin b/vendor/big.bin\nnew file mode 100644\nindex 0000000..2222222\nBinary files /dev/null and b/vendor/big.bin differ\n";
    let mut contributors = largest_diff_contributors(diff, "repo");
    assert_eq!(contributors.len(), 2);
    assert!(
        contributors
            .iter()
            .any(|(path, _)| path == "repo/small.txt")
    );
    assert!(
        contributors
            .iter()
            .any(|(path, _)| path == "repo/vendor/big.bin")
    );
    let (_, big_bytes) = contributors
        .iter()
        .find(|(path, _)| path == "repo/vendor/big.bin")
        .unwrap();
    assert!(*big_bytes > 0);
    let detail = describe_largest_contributors(&mut contributors);
    assert!(detail.contains("repo/vendor/big.bin ("));
    assert!(detail.contains("bytes"));
    // No contributors at all still produces a bounded, honest message rather
    // than an empty one.
    let mut none = Vec::new();
    assert_eq!(
        describe_largest_contributors(&mut none),
        "none identifiable"
    );
    // Bounded to a handful of entries even with many changed files.
    let mut many: Vec<(String, u64)> = (0..20).map(|n| (format!("file{n}"), n)).collect();
    assert_eq!(
        describe_largest_contributors(&mut many).split(", ").count(),
        5
    );
}

/// The patch must not carry what the agent generated while working.
///
/// Patch collection is a `git diff` over the workspace, and a snapshot rooted
/// at a subdirectory has no `.gitignore` at the workspace root — so bytecode,
/// virtualenvs and package caches were encoded into the diff as base64 binary
/// literals. One measured patch was 86% Python bytecode; another job built a
/// virtualenv in the workspace and produced a diff the collector could not
/// buffer, returning nothing at all after 55 minutes.
///
/// This exercises the same git invocation `collect_patches` builds, against a
/// real repository.
#[tokio::test]
async fn generated_files_stay_out_of_the_patch_unless_the_consumer_asks() {
    use crate::git::git_ok;
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path();
    let excludes = work.join("poundlock-exclude");
    std::fs::write(&excludes, sandbox::DEFAULT_PATCH_EXCLUDES).unwrap();

    git_ok(work, &["init", "--quiet"]).await.unwrap();
    git_ok(work, &["config", "user.name", "t"]).await.unwrap();
    git_ok(work, &["config", "user.email", "t@t"])
        .await
        .unwrap();
    std::fs::write(work.join("app.py"), "x = 1\n").unwrap();
    git_ok(work, &["add", "--force", "--all"]).await.unwrap();
    git_ok(
        work,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    )
    .await
    .unwrap();

    // The agent edits real source, and running its tests leaves bytecode and
    // a virtualenv behind.
    std::fs::write(work.join("app.py"), "x = 2\n").unwrap();
    std::fs::create_dir_all(work.join("__pycache__")).unwrap();
    std::fs::write(
        work.join("__pycache__/app.cpython-314.pyc"),
        [0u8, 159, 146, 150],
    )
    .unwrap();
    std::fs::create_dir_all(work.join(".venv/bin")).unwrap();
    std::fs::write(work.join(".venv/bin/python"), "#!/bin/sh\n").unwrap();
    std::fs::create_dir_all(work.join("node_modules/dep")).unwrap();
    std::fs::write(work.join("node_modules/dep/index.js"), "module.exports=1\n").unwrap();

    let excludes_arg = format!("core.excludesFile={}", excludes.display());
    async fn diff(dir: &std::path::Path, excludes_arg: &str) -> String {
        crate::git::git_ok(dir, &["-c", excludes_arg, "add", "--all"])
            .await
            .unwrap();
        crate::git::git_text(dir, &["diff", "--cached", "--binary", "HEAD", "--"])
            .await
            .unwrap()
    }

    // First without the floor, so the rest of this test cannot pass vacuously:
    // this is the behaviour a consumer actually measured.
    let empty = work.join("empty-exclude");
    std::fs::write(&empty, "").unwrap();
    let unfiltered = diff(work, &format!("core.excludesFile={}", empty.display())).await;
    assert!(
        unfiltered.contains("__pycache__"),
        "without the floor the junk lands in the patch"
    );
    assert!(
        unfiltered.contains("GIT binary patch"),
        "and is encoded as a binary literal"
    );
    git_ok(work, &["reset", "--quiet", "HEAD", "--"])
        .await
        .unwrap();

    let patch = diff(work, &excludes_arg).await;
    assert!(
        patch.contains("app.py"),
        "the real edit must survive: {patch}"
    );
    for junk in ["__pycache__", ".pyc", ".venv", "node_modules"] {
        assert!(
            !patch.contains(junk),
            "{junk} must not reach the patch:\n{patch}"
        );
    }
    assert!(
        !patch.contains("GIT binary patch"),
        "no generated binary should be encoded:\n{patch}"
    );

    // The floor is a default, not a policy: an in-tree .gitignore outranks
    // `core.excludesFile`, so a consumer who wants one of these back can say
    // so and git will honour it.
    git_ok(work, &["reset", "--quiet", "HEAD", "--"])
        .await
        .unwrap();
    std::fs::write(work.join(".gitignore"), "!__pycache__/\n!*.py[cod]\n").unwrap();
    let patch = diff(work, &excludes_arg).await;
    assert!(
        patch.contains("__pycache__"),
        "an explicit re-include must win:\n{patch}"
    );
}

/// A diff too large to buffer must still say what made it large.
///
/// The 128 MiB pipe cap is hit while buffering, strictly before the friendly
/// patch-budget diagnostic can run, so the attempt returned an empty patch and
/// no verification report — a consumer lost 55 minutes to one and got back
/// only the byte limit.
#[test]
fn an_undiffable_tree_names_its_largest_paths() {
    use sandbox::describe_numstat;
    let listing = b"12\t4\tsrc/app.py\n0\t0\t.venv/lib/libpython.so\n900\t100\tvendor/bundle.js\n3\t1\tREADME.md\n";
    let described = describe_numstat(listing, "repo");
    assert!(
        described.starts_with("repo/vendor/bundle.js (1000 lines)"),
        "{described}"
    );
    assert!(
        described.contains("repo/src/app.py (16 lines)"),
        "{described}"
    );
    // A binary blob reports `-` rather than counts, and is usually the reason
    // the tree got large — name it rather than dropping it.
    assert!(
        described.contains("repo/.venv/lib/libpython.so (binary)"),
        "{described}"
    );
    assert_eq!(
        described.matches(", ").count(),
        3,
        "at most five: {described}"
    );
    assert_eq!(describe_numstat(b"", "repo"), "");
}

/// The collector owns the `/verify` tmpfs, so it has to outlive the checks
/// that read from it — including the bounded window verification gets after a
/// wall-deadline cutoff. Created 60s past the deadline, it exited first, and
/// a later check mounted an empty `/verify` and was reported
/// `verification_failed`: a policy failure for an infrastructure cause.
#[test]
fn the_collector_outlives_the_verification_window_it_serves() {
    use crate::job_policy::{JobPolicy, VerificationCommand};
    use sandbox::verification_grace_seconds;
    let policy = |timeouts: &[u64]| -> JobPolicy {
        let commands = timeouts
            .iter()
            .map(|seconds| {
                serde_json::from_value::<VerificationCommand>(
                    serde_json::json!({"command": "true", "timeout_seconds": seconds}),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({"verification": {"commands": commands}})).unwrap()
    };

    // Two 300s checks: the window is their sum plus a margin, which is well
    // past the 60 seconds the collector used to get.
    let two_long_checks = verification_grace_seconds(&policy(&[300, 300]));
    assert_eq!(two_long_checks, 630);
    assert!(two_long_checks > 60);

    // A job with no checks still gets a floor rather than zero.
    assert_eq!(verification_grace_seconds(&policy(&[])), 90);
}

#[test]
fn a_fetch_failure_reports_one_bounded_printable_line() {
    use sandbox::fetch_failure;
    assert_eq!(
        fetch_failure(b"noise\nGit checkout exceeds the 50 MiB or 10,000-file limit.\n\n"),
        "Git source: Git checkout exceeds the 50 MiB or 10,000-file limit."
    );
    let hostile = [b"x\x1b[2Jy".as_slice(), &[b'z'; 1000]].concat();
    let line = fetch_failure(&hostile);
    assert!(!line.contains('\x1b'));
    assert!(line.chars().count() <= "Git source: ".len() + 300);
    assert_eq!(fetch_failure(b""), "Git source: Git fetch failed");
}
