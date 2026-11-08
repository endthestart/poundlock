use super::*;

#[test]
fn patch_paths_list_each_file_once() {
    let patch = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/old name.txt b/new name.txt\nrename from old name.txt\n";
    assert_eq!(
        patch_paths(patch).unwrap(),
        ["src/a.rs", "old name.txt", "new name.txt"]
    );
    assert!(patch_paths("").unwrap().is_empty());
    // Every place a path can hide is read, and an unreadable header is refused.
    let renamed = "diff --git a/x b/y\nsimilarity index 100%\nrename from x\nrename to .env\n";
    assert!(patch_paths(renamed).unwrap().contains(&".env".to_string()));
    let added = "diff --git a/new b/new\nnew file mode 100644\n--- /dev/null\n+++ b/new\n";
    assert_eq!(patch_paths(added).unwrap(), ["new"]);
    assert!(patch_paths("diff --git \"a/\\tx\" \"b/\\tx\"\n").is_err());
    assert!(patch_paths("diff --git x y\n").is_err());
    let sql = "diff --git a/q.sql b/q.sql\n--- a/q.sql\n+++ b/q.sql\n@@ -1,2 +1,1 @@\n--- ../../comment\n select 1;\n";
    assert_eq!(
        patch_paths(sql).unwrap(),
        ["q.sql"],
        "hunk lines are content"
    );
}

#[test]
fn job_ids_cannot_change_the_url() {
    assert!(job_path("3f2b8c1e-1d2c-4b9f-9c1a-0d6c2e7f5a10").is_ok());
    for bad in ["", "../admin", "a/b", "a?b", "x".repeat(65).as_str()] {
        assert!(job_path(bad).is_err(), "{bad}");
    }
}

#[test]
fn long_output_keeps_its_end() {
    assert_eq!(tail("short", 10), "short");
    let cut = tail("abcdefghij", 3);
    assert!(cut.ends_with("\nhij"));
    assert!(cut.contains("7 earlier characters omitted"));
}

#[test]
fn the_workspace_admits_only_its_roots() {
    let allowed = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::create_dir(allowed.path().join("project")).unwrap();
    let workspace = Workspace::new(&[allowed.path().to_path_buf()]).unwrap();
    assert!(
        workspace
            .resolve(&allowed.path().join("project").to_string_lossy())
            .is_ok()
    );
    assert!(workspace.resolve(&other.path().to_string_lossy()).is_err());
    assert!(workspace.resolve("relative/path").is_err());
    let escape = allowed.path().join("project/../..");
    assert!(workspace.resolve(&escape.to_string_lossy()).is_err());
    #[cfg(unix)]
    {
        let link = allowed.path().join("link");
        std::os::unix::fs::symlink(other.path(), &link).unwrap();
        assert!(
            workspace.resolve(&link.to_string_lossy()).is_err(),
            "a link out of a root is resolved, then refused"
        );
    }
    assert!(Workspace::new(&[]).is_err());
}

#[test]
fn every_tool_has_a_closed_object_schema() {
    let tools = definitions();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "list_models",
            "delegate_task",
            "job_status",
            "wait_for_job",
            "job_result",
            "get_patch",
            "apply_patch",
            "cancel_job",
            "provenance"
        ]
    );
    for tool in tools.as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert!(!tool["description"].as_str().unwrap().is_empty());
    }
}
