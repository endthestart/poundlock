use super::operations::Transport;
use super::{NodeClient, NodeCredential};
use crate::crypto::sha256_hex_file;
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PREFLIGHT_REPORT_BYTES: u64 = 64 * 1024;

/// Base64 of the raw 32-byte Ed25519 key that signs node update manifests,
/// embedded at build time. A build with a key installs only a binary named by
/// a manifest signed with it; a build without one trusts the controller's
/// checksum, and only over HTTPS or loopback. See docs/node-update-signing.md.
const UPDATE_PUBLIC_KEY: Option<&str> = option_env!("POUNDLOCK_UPDATE_PUBLIC_KEY");

#[derive(Debug, Default, Deserialize)]
struct UpdateInfo {
    available: bool,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    message: Option<String>,
    /// The exact bytes the release key signed.
    #[serde(default)]
    signed_manifest: Option<String>,
    /// Base64 Ed25519 signature over `signed_manifest`.
    #[serde(default)]
    signature: Option<String>,
}

/// What a release signs for one platform's node binary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedManifest {
    os: String,
    arch: String,
    version: String,
    sha256: String,
}

/// An update this node has agreed to download, check and install.
#[derive(Debug, PartialEq)]
struct Approved {
    version: semver::Version,
    sha256: String,
}

/// The update key this binary was built with, as embedded.
pub(super) fn embedded_key_text() -> Option<&'static str> {
    UPDATE_PUBLIC_KEY
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn embedded_key() -> Result<Option<[u8; 32]>> {
    decode_key(embedded_key_text())
}

fn decode_key(encoded: Option<&str>) -> Result<Option<[u8; 32]>> {
    let Some(encoded) = encoded.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let bytes = STANDARD
        .decode(encoded)
        .context("the update signing key is not base64")?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("the update signing key is not a 32-byte Ed25519 public key"))?;
    Ok(Some(key))
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(value) = std::env::var("POUNDLOCK_NODE_DATA_DIR")
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }
    match std::env::consts::OS {
        "macos" => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/var/lib/poundlock".into());
            PathBuf::from(home).join(".local/share/poundlock")
        }
        _ => PathBuf::from("/var/lib/poundlock"),
    }
}

pub(super) fn install_binary_path(data_dir: &Path) -> PathBuf {
    if std::env::consts::OS == "macos" {
        data_dir.join("bin").join("poundlock")
    } else {
        data_dir.join("poundlock")
    }
}

pub(super) async fn maybe_replace(
    api: &NodeClient,
    credential: &NodeCredential,
    data_dir: &Path,
    config_file: &Path,
) -> Result<bool> {
    replace_with_key(api, credential, data_dir, config_file, embedded_key()?).await
}

async fn replace_with_key(
    api: &NodeClient,
    credential: &NodeCredential,
    data_dir: &Path,
    config_file: &Path,
    key: Option<[u8; 32]>,
) -> Result<bool> {
    // Without a signature the controller's checksum is the only proof, so it
    // must come over a connection nobody on the network can answer for.
    if key.is_none() {
        ensure!(
            super::operations::transport(api.base())? != Transport::PlainHttp,
            "Automatic update refused: the controller address uses plain HTTP. Use an https:// \
             controller address, or a node build that verifies signed updates."
        );
    }
    let info: UpdateInfo = serde_json::from_value(api.update_info(credential).await?)
        .context("decode update manifest")?;
    if !info.available {
        if let Some(message) = info.message {
            tracing::info!(%message, "node binary update is not available");
        }
        return Ok(false);
    }
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    let Some(approved) = approve(&info, key.as_ref(), &current)? else {
        return Ok(false);
    };
    if info.size.is_some_and(|size| size > MAX_ARTIFACT_BYTES) {
        bail!("update is larger than the size cap");
    }
    let expected = approved.sha256;
    let target = replace_path(data_dir);
    if target.exists() && sha256_hex_file(&target).ok().as_deref() == Some(expected.as_str()) {
        return Ok(false);
    }
    let tmp = target.with_extension("new");
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("create node data directory")?;
    }
    api.download_update(credential, &tmp, MAX_ARTIFACT_BYTES)
        .await?;
    let actual = tokio::task::spawn_blocking({
        let tmp = tmp.clone();
        move || sha256_hex_file(&tmp)
    })
    .await
    .context("join sha256 task")??;
    if actual != expected {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("downloaded node binary failed sha256 verification");
    }
    set_executable(&tmp).await?;
    // Refuse an incompatible executable/configuration before replacing a working node.
    if let Err(error) = preflight(&tmp, config_file, key.as_ref()).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error);
    }
    if target.exists() {
        tokio::fs::copy(&target, target.with_extension("previous")).await?;
    }
    tokio::fs::rename(&tmp, &target)
        .await
        .context("replace node binary")?;
    tracing::info!(version = %approved.version, "node binary update installed");
    Ok(true)
}

/// Decides whether an offered update may be installed. `Ok(None)` means there
/// is nothing newer; every refusal is an error, so it is logged.
fn approve(
    info: &UpdateInfo,
    key: Option<&[u8; 32]>,
    current: &semver::Version,
) -> Result<Option<Approved>> {
    let (version, sha256) = match key {
        Some(key) => {
            let (Some(manifest), Some(signature)) = (&info.signed_manifest, &info.signature) else {
                bail!(
                    "The controller offered an unsigned update; this node installs only signed updates."
                );
            };
            let signature = STANDARD
                .decode(signature.trim())
                .context("update signature is not base64")?;
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
                .verify(manifest.as_bytes(), &signature)
                .map_err(|_| {
                    anyhow!("update manifest signature is not valid for this node's key")
                })?;
            let manifest: SignedManifest =
                serde_json::from_str(manifest).context("decode signed update manifest")?;
            ensure!(
                manifest.os == std::env::consts::OS && manifest.arch == std::env::consts::ARCH,
                "signed update is for {}/{}, not this machine",
                manifest.os,
                manifest.arch
            );
            (manifest.version, manifest.sha256)
        }
        None => (
            info.version
                .clone()
                .ok_or_else(|| anyhow!("update manifest did not include a version"))?,
            info.sha256
                .clone()
                .ok_or_else(|| anyhow!("update manifest did not include a sha256"))?,
        ),
    };
    ensure!(
        sha256.len() == 64
            && sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "update manifest did not include a valid sha256"
    );
    let version = semver::Version::parse(version.trim()).context("update version is invalid")?;
    match version.cmp(current) {
        Ordering::Equal => Ok(None),
        Ordering::Less => bail!("refusing to downgrade the node from {current} to {version}"),
        Ordering::Greater => Ok(Some(Approved { version, sha256 })),
    }
}

/// Runs the new binary's `node-check` against this node's configuration. A
/// build that verifies signed updates also requires its replacement to carry
/// the same key, so an update can never switch verification off.
async fn preflight(binary: &Path, config_file: &Path, key: Option<&[u8; 32]>) -> Result<()> {
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("node-check")
        .arg("--config")
        .arg(config_file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    crate::process::isolate(&mut command);
    let mut child = command.spawn().context("start update preflight")?;
    let _group = crate::process::ProcessGroup::new(&child)?;
    let mut stdout = child.stdout.take().context("capture update preflight")?;
    let probe = tokio::time::timeout(Duration::from_secs(10), async {
        let mut report = Vec::new();
        (&mut stdout)
            .take(MAX_PREFLIGHT_REPORT_BYTES)
            .read_to_end(&mut report)
            .await?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, report))
    })
    .await;
    let Ok(Ok((status, report))) = probe else {
        bail!("Downloaded update failed configuration preflight; installed binary retained");
    };
    ensure!(
        status.success(),
        "Downloaded update failed configuration preflight; installed binary retained"
    );
    if let Some(key) = key {
        let reported: Value = serde_json::from_slice(&report).unwrap_or_default();
        let carried = decode_key(reported["update_key"].as_str()).ok().flatten();
        ensure!(
            carried.as_ref() == Some(key),
            "Downloaded update does not verify updates with this node's signing key; installed binary retained"
        );
    }
    Ok(())
}

fn replace_path(data_dir: &Path) -> PathBuf {
    if let Ok(current) = std::env::current_exe()
        && current.starts_with(data_dir)
    {
        return current;
    }
    install_binary_path(data_dir)
}

async fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .await
            .context("set node binary permissions")?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "update_tests.rs"]
mod tests;
