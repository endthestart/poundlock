use super::secrets::wildcard;
use super::*;

#[test]
fn credential_files_are_recognised_wherever_they_are() {
    for secret in [
        ".env",
        "app/.env.local",
        "deploy/server.pem",
        "keys/id_ed25519",
        "home/.ssh/config",
        "config/secrets/db.txt",
        "user/.aws/credentials",
        "ops/.kube/config",
        "gcp-service-account-prod.json",
        "infra/prod.tfstate",
        ".npmrc",
    ] {
        assert!(is_secret(secret), "{secret} should be a secret");
    }
    for ordinary in [
        ".env.example",
        "src/main.rs",
        "docs/keys.md",
        "secrets.md",
        "config/credentials.txt",
        "web/config.json",
        "aws/credentials-guide.md",
    ] {
        assert!(!is_secret(ordinary), "{ordinary} is not a secret");
    }
}

#[test]
fn wildcards_stay_within_one_name() {
    assert!(wildcard("*.pem", "a.pem"));
    assert!(wildcard(
        "*service-account*.json",
        "my-service-account-key.json"
    ));
    assert!(wildcard("id_rsa.*", "id_rsa.pub"));
    assert!(!wildcard("*.pem", "a.pem.txt"));
    assert!(wildcard("?.txt", "a.txt"));
    assert!(!wildcard("?.txt", "ab.txt"));
}

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, content).unwrap();
}

#[test]
fn a_plain_folder_skips_builds_secrets_and_links() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "src/lib.rs", "pub fn a() {}\n");
    write(root, "README.md", "hi\n");
    write(root, "node_modules/x/index.js", "x\n");
    write(root, "target/debug/out", "x\n");
    write(root, ".env", "TOKEN=1\n");
    write(root, ".env.example", "TOKEN=\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", root.join("hosts")).unwrap();
    let snapshot = collect(root).unwrap();
    let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, [".env.example", "README.md", "src/lib.rs"]);
    assert_eq!(snapshot.secrets, [".env"]);
    assert!(!snapshot.from_git);
    #[cfg(unix)]
    assert_eq!(snapshot.skipped, [("hosts".to_string(), "symbolic_link")]);
}

#[test]
fn a_git_folder_follows_its_ignore_rules_and_keeps_untracked_work() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let git = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    git(&["init", "-q"]);
    write(root, ".gitignore", "generated/\n");
    write(root, "src/main.rs", "fn main() {}\n");
    write(root, "build/tracked.txt", "kept because Git tracks it\n");
    git(&["add", "."]);
    write(root, "src/new.rs", "// untracked work in progress\n");
    write(root, "generated/out.txt", "ignored\n");
    write(root, "certs/server.key", "secret\n");
    let snapshot = collect(root).unwrap();
    let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            ".gitignore",
            "build/tracked.txt",
            "src/main.rs",
            "src/new.rs"
        ]
    );
    assert_eq!(snapshot.secrets, ["certs/server.key"]);
    assert!(snapshot.from_git);
}

#[test]
fn the_digest_changes_with_content_and_executability() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "run.sh", "echo hi\n");
    let first = collect(root).unwrap().digest;
    assert_eq!(collect(root).unwrap().digest, first);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let snapshot = collect(root).unwrap();
        assert!(snapshot.files[0].executable);
        assert_ne!(snapshot.digest, first);
    }
    write(root, "run.sh", "echo bye\n");
    assert_ne!(collect(root).unwrap().digest, first);
}

#[test]
fn an_empty_folder_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    assert!(collect(directory.path()).is_err());
}
