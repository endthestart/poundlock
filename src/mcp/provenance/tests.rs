use super::*;

fn git(directory: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

fn write(directory: &Path, path: &str, text: &str) {
    let full = directory.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

fn patch<'a>(job: &'a str, files: &'a [String], verification: Option<&'a str>) -> JobPatch<'a> {
    JobPatch {
        job_id: job,
        attempt: None,
        index: 0,
        patch: job,
        files,
        verification,
    }
}

fn apply(directory: &Path, job: &str, change: impl FnOnce()) {
    let pending = begin(directory).unwrap().unwrap();
    change();
    let files = ["x".to_owned()];
    finish(pending, directory, patch(job, &files, Some("passed"))).unwrap();
}

#[test]
fn finds_every_change_that_did_not_come_from_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    write(root, "src/lib.rs", "a\n");
    write(root, ".env", "SECRET=1\n");
    // Before any patch, a folder has no record and nothing to report.
    let empty = report(root).unwrap();
    assert!(empty.applied.is_empty() && empty.clean());
    assert!(empty.text().contains("No job patches"));

    apply(root, "job-1", || write(root, "src/lib.rs", "b\n"));
    apply(root, "job-2", || write(root, "src/new.rs", "c\n"));
    let clean = report(root).unwrap();
    assert_eq!(clean.applied.len(), 2);
    assert!(clean.clean(), "{}", clean.text());
    assert!(clean.text().contains("Every change since the first one"));
    assert!(clean.text().contains("Each patch's job passed its checks."));

    // A credential file changing is not code; the record never sees it.
    write(root, ".env", "SECRET=2\n");
    assert!(report(root).unwrap().clean());

    // The agent's own edit, then another job, then a stray file.
    write(root, "src/lib.rs", "by hand\n");
    apply(root, "job-3", || write(root, "src/third.rs", "d\n"));
    write(root, "notes.txt", "e\n");
    std::fs::remove_file(root.join("src/new.rs")).unwrap();
    let found = report(root).unwrap();
    assert_eq!(
        found.outside,
        vec![
            OutsideChange {
                after_job: "job-2".into(),
                before_job: Some("job-3".into()),
                added: vec![],
                changed: vec!["src/lib.rs".into()],
                removed: vec![],
            },
            OutsideChange {
                after_job: "job-3".into(),
                before_job: None,
                added: vec!["notes.txt".into()],
                changed: vec![],
                removed: vec!["src/new.rs".into()],
            },
        ]
    );
    let text = found.text();
    assert!(text.contains("between job job-2 and job job-3: changed src/lib.rs"));
    assert!(text.contains("since job job-3: added notes.txt; removed src/new.rs"));

    // The record holds hashes and paths, never contents.
    let records = location(root).unwrap().unwrap().0;
    let all: String = std::fs::read_dir(records.join("manifests"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(!all.contains("by hand") && !all.contains(".env"));
}

#[test]
fn names_patches_applied_without_passing_checks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    write(root, "a.rs", "a\n");
    apply(root, "job-1", || write(root, "a.rs", "b\n"));
    for (job, status) in [("job-2", Some("failed")), ("job-3", None)] {
        let pending = begin(root).unwrap().unwrap();
        write(root, &format!("{job}.rs"), "c\n");
        finish(pending, root, patch(job, &[], status)).unwrap();
    }
    let found = report(root).unwrap();
    assert!(found.clean(), "{}", found.text());
    assert_eq!(
        found
            .without_evidence()
            .iter()
            .map(|entry| entry.job_id.as_str())
            .collect::<Vec<_>>(),
        ["job-2", "job-3"]
    );
    assert!(
        found
            .text()
            .contains("Applied without passing checks: job job-2 (failed), job job-3 (not known).")
    );
}

#[test]
fn keeps_each_folder_of_a_repository_apart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    write(root, "app/main.rs", "a\n");
    write(root, "other/main.rs", "a\n");
    let app = root.join("app");
    apply(&app, "job-1", || write(&app, "main.rs", "b\n"));
    write(root, "other/main.rs", "changed elsewhere\n");
    let found = report(&app).unwrap();
    assert_eq!(found.applied[0].directory, "app");
    assert!(found.clean(), "{}", found.text());
    assert!(report(&root.join("other")).unwrap().applied.is_empty());
}

#[test]
fn records_nothing_outside_git() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.txt", "a\n");
    assert!(begin(dir.path()).unwrap().is_none());
    assert!(report(dir.path()).unwrap().applied.is_empty());
}
