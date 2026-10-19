//! Node self-update trust: what a node agrees to install, and that a refused
//! update is never downloaded, run or installed.
use super::*;
use crate::node::network::NetworkPolicy;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const OS: &str = std::env::consts::OS;
const ARCH: &str = std::env::consts::ARCH;

fn keypair(seed: u8) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap()
}

fn public(pair: &Ed25519KeyPair) -> [u8; 32] {
    pair.public_key().as_ref().try_into().unwrap()
}

fn manifest(version: &str, sha256: &str) -> String {
    json!({"os":OS,"arch":ARCH,"version":version,"sha256":sha256}).to_string()
}

fn signed(pair: &Ed25519KeyPair, manifest: &str) -> UpdateInfo {
    UpdateInfo {
        available: true,
        signed_manifest: Some(manifest.into()),
        signature: Some(STANDARD.encode(pair.sign(manifest.as_bytes()))),
        ..UpdateInfo::default()
    }
}

fn unsigned(version: &str, sha256: &str) -> UpdateInfo {
    UpdateInfo {
        available: true,
        version: Some(version.into()),
        sha256: Some(sha256.into()),
        ..UpdateInfo::default()
    }
}

fn v(version: &str) -> semver::Version {
    semver::Version::parse(version).unwrap()
}

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn a_node_with_a_key_rejects_an_unsigned_manifest() {
    let key = public(&keypair(1));
    let error = approve(&unsigned("9.0.0", SHA), Some(&key), &v("0.1.2")).unwrap_err();
    assert!(error.to_string().contains("unsigned"), "{error:#}");
}

#[test]
fn a_node_with_a_key_rejects_other_keys_and_altered_manifests() {
    let pair = keypair(1);
    let key = public(&pair);
    let other = signed(&keypair(2), &manifest("9.0.0", SHA));
    assert!(approve(&other, Some(&key), &v("0.1.2")).is_err());

    let mut altered = signed(&pair, &manifest("9.0.0", SHA));
    altered.signed_manifest = Some(manifest("9.0.0", &"f".repeat(64)));
    assert!(approve(&altered, Some(&key), &v("0.1.2")).is_err());

    let mut garbled = signed(&pair, &manifest("9.0.0", SHA));
    garbled.signature = Some("not base64!".into());
    assert!(approve(&garbled, Some(&key), &v("0.1.2")).is_err());
}

#[test]
fn a_signed_manifest_binds_the_platform() {
    let pair = keypair(1);
    let other_os = if OS == "linux" { "macos" } else { "linux" };
    let elsewhere = json!({"os":other_os,"arch":ARCH,"version":"9.0.0","sha256":SHA}).to_string();
    let error = approve(
        &signed(&pair, &elsewhere),
        Some(&public(&pair)),
        &v("0.1.2"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("not this machine"), "{error:#}");
    // Extra, unsigned-looking fields are not part of the format.
    let extra = json!({"os":OS,"arch":ARCH,"version":"9.0.0","sha256":SHA,"url":"x"}).to_string();
    assert!(approve(&signed(&pair, &extra), Some(&public(&pair)), &v("0.1.2")).is_err());
}

#[test]
fn only_the_signed_checksum_is_trusted() {
    let pair = keypair(1);
    let mut info = signed(&pair, &manifest("9.0.0", SHA));
    info.sha256 = Some("e".repeat(64));
    info.version = Some("99.0.0".into());
    assert_eq!(
        approve(&info, Some(&public(&pair)), &v("0.1.2")).unwrap(),
        Some(Approved {
            version: v("9.0.0"),
            sha256: SHA.into()
        })
    );
}

#[test]
fn a_downgrade_is_rejected_and_the_same_version_is_not_an_update() {
    let pair = keypair(1);
    let key = public(&pair);
    for info in [
        signed(&pair, &manifest("0.1.1", SHA)),
        unsigned("0.1.1", SHA),
    ] {
        let key = info.signature.as_ref().map(|_| &key);
        let error = approve(&info, key, &v("0.1.2")).unwrap_err();
        assert!(error.to_string().contains("downgrade"), "{error:#}");
    }
    // A pre-release sorts before its release.
    assert!(approve(&unsigned("0.1.2-rc.1", SHA), None, &v("0.1.2")).is_err());
    assert_eq!(
        approve(
            &signed(&pair, &manifest("0.1.2", SHA)),
            Some(&key),
            &v("0.1.2")
        )
        .unwrap(),
        None
    );
    assert_eq!(
        approve(&unsigned("0.1.2", SHA), None, &v("0.1.2")).unwrap(),
        None
    );
    assert!(
        approve(&unsigned("0.1.3", SHA), None, &v("0.1.2"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_manifest_without_a_version_or_a_real_checksum_is_rejected() {
    let mut info = unsigned("0.1.3", SHA);
    info.version = None;
    assert!(approve(&info, None, &v("0.1.2")).is_err());
    for bad in ["", "abc", &SHA.to_uppercase(), &format!("{}g", &SHA[..63])] {
        assert!(
            approve(&unsigned("0.1.3", bad), None, &v("0.1.2")).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn the_embedded_key_must_be_a_32_byte_base64_key() {
    assert_eq!(decode_key(None).unwrap(), None);
    assert_eq!(decode_key(Some("  ")).unwrap(), None);
    let key = public(&keypair(3));
    assert_eq!(decode_key(Some(&STANDARD.encode(key))).unwrap(), Some(key));
    assert!(decode_key(Some("not-base64")).is_err());
    assert!(decode_key(Some(&STANDARD.encode([0u8; 31]))).is_err());
}

#[test]
fn only_loopback_counts_as_a_local_plain_http_controller() {
    use super::super::operations::transport;
    for local in [
        "http://127.0.0.1:7331",
        "http://127.8.9.10",
        "http://localhost:7331",
        "http://LOCALHOST",
        "http://[::1]:7331",
    ] {
        assert_eq!(transport(local).unwrap(), Transport::Loopback, "{local}");
    }
    for remote in [
        "http://10.20.0.5:7331",
        "http://controller.lan:7331",
        "http://127.0.0.1.example.test",
        "http://localhost.example.test",
        "http://[::ffff:10.20.0.5]",
        "http://0.0.0.0:7331",
    ] {
        assert_eq!(transport(remote).unwrap(), Transport::PlainHttp, "{remote}");
    }
    assert_eq!(
        transport("https://controller.lan").unwrap(),
        Transport::Https
    );
}

/// A controller that offers `offer` and serves `binary`, counting downloads.
struct Controller {
    url: String,
    downloads: Arc<AtomicUsize>,
}

async fn controller(offer: serde_json::Value, binary: Vec<u8>) -> Controller {
    #[derive(Clone)]
    struct Served {
        offer: serde_json::Value,
        binary: Arc<Vec<u8>>,
        downloads: Arc<AtomicUsize>,
    }
    let downloads = Arc::new(AtomicUsize::new(0));
    let served = Served {
        offer,
        binary: Arc::new(binary),
        downloads: downloads.clone(),
    };
    let app = Router::new()
        .route(
            "/node/v1/nodes/{id}/update",
            get(|State(served): State<Served>| async move { axum::Json(served.offer) }),
        )
        .route(
            "/node/v1/nodes/{id}/update/binary",
            get(|State(served): State<Served>| async move {
                served.downloads.fetch_add(1, Ordering::SeqCst);
                served.binary.as_ref().clone()
            }),
        )
        .with_state(served);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    Controller { url, downloads }
}

/// A stand-in node binary whose `node-check` succeeds and reports `key`.
fn fake_binary(key: Option<&[u8; 32]>) -> Vec<u8> {
    let report = json!({"configuration":"valid","update_key":key.map(|key| STANDARD.encode(key))});
    format!("#!/bin/sh\nprintf '%s\\n' '{report}'\n").into_bytes()
}

struct Node {
    directory: tempfile::TempDir,
}

impl Node {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("node.toml"), "").unwrap();
        Self { directory }
    }

    fn installed(&self) -> PathBuf {
        install_binary_path(self.directory.path())
    }

    async fn update(&self, controller: &str, key: Option<[u8; 32]>) -> Result<bool> {
        let api = NodeClient::new(controller.into(), &NetworkPolicy::default()).unwrap();
        let credential = NodeCredential {
            node_id: "node".into(),
            credential: "credential".into(),
        };
        replace_with_key(
            &api,
            &credential,
            self.directory.path(),
            &self.directory.path().join("node.toml"),
            key,
        )
        .await
    }
}

fn offer(info: &UpdateInfo) -> serde_json::Value {
    json!({
        "available": info.available,
        "version": info.version,
        "sha256": info.sha256,
        "signed_manifest": info.signed_manifest,
        "signature": info.signature,
    })
}

#[tokio::test]
async fn an_update_over_plain_http_is_refused_before_any_request() {
    let node = Node::new();
    // TEST-NET-1: nothing answers there, so reaching the network would hang.
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        node.update("http://192.0.2.1:7331", None),
    )
    .await
    .expect("refused without a request")
    .unwrap_err();
    assert!(error.to_string().contains("plain HTTP"), "{error:#}");
    assert!(!node.installed().exists());
}

#[tokio::test]
async fn an_unsigned_update_over_loopback_installs_a_newer_version() {
    let binary = fake_binary(None);
    let sha = crate::crypto::sha256_hex(&binary);
    let controller = controller(offer(&unsigned("99.0.0", &sha)), binary.clone()).await;
    let node = Node::new();
    assert!(node.update(&controller.url, None).await.unwrap());
    assert_eq!(std::fs::read(node.installed()).unwrap(), binary);
}

#[tokio::test]
async fn a_node_with_a_key_never_downloads_an_unsigned_or_older_update() {
    let pair = keypair(4);
    let key = public(&pair);
    let binary = fake_binary(Some(&key));
    let sha = crate::crypto::sha256_hex(&binary);
    for info in [
        unsigned("99.0.0", &sha),
        signed(&keypair(5), &manifest("99.0.0", &sha)),
        signed(&pair, &manifest("0.0.1", &sha)),
    ] {
        let controller = controller(offer(&info), binary.clone()).await;
        let node = Node::new();
        assert!(node.update(&controller.url, Some(key)).await.is_err());
        assert_eq!(controller.downloads.load(Ordering::SeqCst), 0);
        assert!(!node.installed().exists());
    }
}

#[tokio::test]
async fn a_signed_update_installs_only_the_signed_bytes() {
    let pair = keypair(6);
    let key = public(&pair);
    let binary = fake_binary(Some(&key));
    let sha = crate::crypto::sha256_hex(&binary);

    let mut swapped = binary.clone();
    swapped.extend_from_slice(b"# swapped\n");
    let controller = controller(offer(&signed(&pair, &manifest("99.0.0", &sha))), swapped).await;
    let node = Node::new();
    let error = node.update(&controller.url, Some(key)).await.unwrap_err();
    assert!(error.to_string().contains("sha256"), "{error:#}");
    assert!(!node.installed().exists());

    let controller = self::controller(
        offer(&signed(&pair, &manifest("99.0.0", &sha))),
        binary.clone(),
    )
    .await;
    assert!(node.update(&controller.url, Some(key)).await.unwrap());
    assert_eq!(std::fs::read(node.installed()).unwrap(), binary);
}

#[tokio::test]
async fn a_signed_update_cannot_switch_signature_checks_off() {
    let pair = keypair(7);
    let key = public(&pair);
    for carried in [None, Some(public(&keypair(8)))] {
        let binary = fake_binary(carried.as_ref());
        let sha = crate::crypto::sha256_hex(&binary);
        let controller = controller(offer(&signed(&pair, &manifest("99.0.0", &sha))), binary).await;
        let node = Node::new();
        let error = node.update(&controller.url, Some(key)).await.unwrap_err();
        assert!(error.to_string().contains("signing key"), "{error:#}");
        assert!(!node.installed().exists());
        assert!(!node.installed().with_extension("new").exists());
    }
}

/// The release script and the node agree on the signed bytes. Skipped where
/// only LibreSSL is installed; the Linux CI images carry OpenSSL 3.
#[test]
fn the_release_signing_script_produces_manifests_nodes_accept() {
    let openssl = std::process::Command::new("openssl")
        .arg("version")
        .output();
    let Ok(version) = openssl else {
        eprintln!("skipped: openssl is not installed");
        return;
    };
    let version = String::from_utf8_lossy(&version.stdout);
    if !(version.starts_with("OpenSSL 3.") || version.starts_with("OpenSSL 4.")) {
        eprintln!("skipped: the signing script needs OpenSSL 3, found {version}");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    // A throwaway key for this test only.
    let key_file = directory.path().join("test-signing-key.pem");
    let generated = std::process::Command::new("openssl")
        .args(["genpkey", "-algorithm", "ed25519", "-out"])
        .arg(&key_file)
        .status()
        .unwrap();
    assert!(generated.success());
    let artifacts = directory.path().join("nodes");
    let platform = artifacts.join(format!("{OS}-{ARCH}"));
    std::fs::create_dir_all(&platform).unwrap();
    std::fs::write(platform.join("poundlock"), b"node binary").unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/sign-node-manifests.sh");
    let output = std::process::Command::new("bash")
        .arg(script)
        .arg(&artifacts)
        .arg("99.1.0")
        // A build's own public key (CI sets it) is not this test key's.
        .env_remove("POUNDLOCK_UPDATE_PUBLIC_KEY")
        // The one-line base64 form kept in CI secrets and the Keychain.
        .env(
            "POUNDLOCK_UPDATE_SIGNING_KEY",
            STANDARD.encode(std::fs::read(&key_file).unwrap()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let printed = stdout
        .lines()
        .find_map(|line| line.strip_prefix("sign-node-manifests: public key "))
        .unwrap();
    let key = decode_key(Some(printed)).unwrap().unwrap();
    let info = UpdateInfo {
        available: true,
        signed_manifest: Some(
            std::fs::read_to_string(platform.join("poundlock.manifest.json")).unwrap(),
        ),
        signature: Some(std::fs::read_to_string(platform.join("poundlock.manifest.sig")).unwrap()),
        ..UpdateInfo::default()
    };
    assert_eq!(
        approve(&info, Some(&key), &v("0.1.2")).unwrap(),
        Some(Approved {
            version: v("99.1.0"),
            sha256: crate::crypto::sha256_hex(b"node binary"),
        })
    );
    assert!(approve(&info, Some(&public(&keypair(9))), &v("0.1.2")).is_err());
}
