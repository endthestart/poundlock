use super::nodes::node_auth;
use super::{ApiError, ApiResult, AppState, admin_auth};
use crate::domain::Node;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;

pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/nodes/binaries", get(list_binaries))
        .route("/api/v1/nodes/update-signatures", put(upload_signatures))
        .route("/api/v1/nodes/binaries/{os}/{arch}", get(download_binary))
        .route("/node/v1/nodes/{id}/update", get(update_info))
        .route("/node/v1/nodes/{id}/update/binary", get(update_binary))
}

async fn update_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    node_auth(&state, &headers, &id)?;
    let node = state
        .store
        .node(&id, state.config.node_offline_seconds)?
        .ok_or_else(|| ApiError::not_found("node not found"))?;
    Ok(Json(manifest(&state, &node)))
}

async fn update_binary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    node_auth(&state, &headers, &id)?;
    let node = state
        .store
        .node(&id, state.config.node_offline_seconds)?
        .ok_or_else(|| ApiError::not_found("node not found"))?;
    let Some(file) = artifact_for(&state, &node) else {
        return Err(ApiError::not_found(
            "no node binary is published for this machine's platform",
        ));
    };
    let bytes = std::fs::read(&file.path)
        .map_err(|_| ApiError::unavailable("the node binary is missing from this controller"))?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(ApiError::unavailable("node binary exceeds the size cap"));
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("x-poundlock-sha256", &file.sha256)
        .body(Body::from(bytes))
        .map_err(|_| ApiError::unavailable("could not serve the node binary"))
}

async fn list_binaries(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let mut platforms = Vec::new();
    for (os, arch) in known_platforms(&state) {
        if let Some(file) = artifact_for_platform(&state, &os, &arch) {
            platforms.push(json!({
                "os": os,
                "arch": arch,
                "available": true,
                "sha256": file.sha256,
                "size": file.size,
                "signed": signed_manifest(&state, &os, &arch, &file.sha256).is_some()
            }));
        } else {
            platforms.push(json!({
                "os": os,
                "arch": arch,
                "available": false
            }));
        }
    }
    Ok(Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"platforms":platforms}),
    ))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadedSignature {
    /// The exact signed bytes: `{"os","arch","version","sha256"}`.
    manifest: String,
    /// Base64 Ed25519 signature over `manifest`.
    signature: String,
}

/// Signatures made by the release signer for the binaries this controller
/// serves. Each must name a served binary and this controller's version; the
/// signature itself is checked by the nodes, not here.
async fn upload_signatures(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(uploads): Json<Vec<UploadedSignature>>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    if uploads.is_empty() || uploads.len() > 16 {
        return Err(ApiError::bad("send between 1 and 16 signed manifests"));
    }
    let mut accepted = Vec::new();
    for upload in uploads {
        if upload.manifest.len() as u64 > MAX_SIGNED_MANIFEST_BYTES
            || upload.signature.len() as u64 > MAX_SIGNED_MANIFEST_BYTES
            || upload.signature.trim().is_empty()
        {
            return Err(ApiError::bad("a signed manifest or signature is malformed"));
        }
        let claims: Value = serde_json::from_str(&upload.manifest)
            .map_err(|_| ApiError::bad("a signed manifest is not JSON"))?;
        let (Some(os), Some(arch), Some(version), Some(sha256)) = (
            claims["os"].as_str(),
            claims["arch"].as_str(),
            claims["version"].as_str(),
            claims["sha256"].as_str(),
        ) else {
            return Err(ApiError::bad(
                "a signed manifest lacks os, arch, version or sha256",
            ));
        };
        if version != env!("CARGO_PKG_VERSION") {
            return Err(ApiError::bad(format!(
                "a signed manifest is for {version}; this controller serves {}",
                env!("CARGO_PKG_VERSION")
            )));
        }
        let served = artifact_for_platform(&state, os, arch);
        if served.as_ref().map(|file| file.sha256.as_str()) != Some(sha256) {
            return Err(ApiError::bad(format!(
                "the signed manifest for {os}/{arch} does not name the binary this controller serves"
            )));
        }
        state.store.save_node_signature(
            os,
            arch,
            sha256,
            &upload.manifest,
            upload.signature.trim(),
        )?;
        accepted.push(json!({"os":os,"arch":arch,"sha256":sha256}));
    }
    Ok(Json(json!({"accepted":accepted})))
}

async fn download_binary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((os, arch)): Path<(String, String)>,
) -> ApiResult<Response> {
    admin_auth(&state, &headers)?;
    let os = os.trim().to_ascii_lowercase();
    let arch = normalize_arch(&arch);
    let Some(file) = artifact_for_platform(&state, &os, &arch) else {
        return Err(ApiError::not_found(
            "no node binary is published for this platform",
        ));
    };
    let bytes = std::fs::read(&file.path)
        .map_err(|_| ApiError::unavailable("the node binary is missing from this controller"))?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(ApiError::unavailable("node binary exceeds the size cap"));
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"poundlock-{os}-{arch}\""),
        )
        .header("x-poundlock-sha256", &file.sha256)
        .body(Body::from(bytes))
        .map_err(|_| ApiError::unavailable("could not serve the node binary"))
}

fn known_platforms(state: &AppState) -> Vec<(String, String)> {
    let mut platforms = vec![
        ("linux".into(), "x86_64".into()),
        ("macos".into(), "aarch64".into()),
        ("macos".into(), "x86_64".into()),
        ("linux".into(), "aarch64".into()),
    ];
    if let Some(dir) = &state.config.client_artifacts_dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some((os, arch)) = name.split_once('-') {
                let pair = (os.to_owned(), normalize_arch(arch));
                if !platforms.contains(&pair) {
                    platforms.push(pair);
                }
            }
        }
    }
    platforms
}

fn manifest(state: &AppState, node: &Node) -> Value {
    let (os, arch) = node_platform(node);
    let version = env!("CARGO_PKG_VERSION");
    if arch.is_empty() {
        return json!({
            "available":false,
            "version":version,
            "os":os,
            "arch":arch,
            "message":"This machine has not reported its architecture yet."
        });
    }
    match artifact_for(state, node) {
        Some(file) => {
            let mut offer = json!({
                "available":true,
                "version":version,
                "sha256":file.sha256,
                "size":file.size,
                "os":os,
                "arch":arch
            });
            // What a node built with the release key requires. The controller
            // only relays it; nodes verify it against their embedded key.
            if let Some(signed) = signed_manifest(state, &os, &arch, &file.sha256) {
                offer["signed_manifest"] = json!(signed.manifest);
                offer["signature"] = json!(signed.signature);
            }
            offer
        }
        None => json!({
            "available":false,
            "version":version,
            "os":os,
            "arch":arch,
            "message":format!("No node binary is published for {os}/{arch} yet.")
        }),
    }
}

fn artifact_for(state: &AppState, node: &Node) -> Option<CachedFile> {
    let (os, arch) = node_platform(node);
    artifact_for_platform(state, &os, &arch)
}

fn artifact_for_platform(state: &AppState, os: &str, arch: &str) -> Option<CachedFile> {
    if !matches!(os, "linux" | "macos") || !matches!(arch, "x86_64" | "aarch64") {
        return None;
    }
    if os == "linux" && arch == "x86_64" {
        // A signed release binary wins over the controller's own copy, so a
        // node that requires signatures is served exactly what was signed.
        // Checked first so an unsigned deployment never hashes a second binary.
        let signed = state
            .config
            .client_artifacts_dir
            .as_ref()
            .is_some_and(|dir| {
                dir.join(format!("{os}-{arch}"))
                    .join("poundlock.manifest.json")
                    .is_file()
            });
        if signed
            && let Some(file) = published_artifact(state, os, arch)
            && signed_manifest(state, os, arch, &file.sha256).is_some()
        {
            return Some(file);
        }
        if let Some(file) = controller_linux_amd64() {
            return Some(file.clone());
        }
    }
    published_artifact(state, os, arch)
}

fn published_artifact(state: &AppState, os: &str, arch: &str) -> Option<CachedFile> {
    let dir = state.config.client_artifacts_dir.as_ref()?;
    let allowed = std::fs::canonicalize(dir).ok()?;
    let path = std::fs::canonicalize(dir.join(format!("{os}-{arch}")).join("poundlock")).ok()?;
    if !path.starts_with(allowed) {
        return None;
    }
    load_file(path)
}

/// The largest signed manifest or signature file the controller will relay.
const MAX_SIGNED_MANIFEST_BYTES: u64 = 4096;

struct SignedManifest {
    manifest: String,
    signature: String,
}

/// The release-signed manifest published beside `<os>-<arch>/poundlock`, when
/// it names the binary actually being served. `scripts/sign-node-manifests.sh`
/// writes these files; see docs/node-update-signing.md.
fn signed_manifest(state: &AppState, os: &str, arch: &str, sha256: &str) -> Option<SignedManifest> {
    let Some(dir) = state.config.client_artifacts_dir.as_ref() else {
        return uploaded_manifest(state, os, arch, sha256);
    };
    let Ok(allowed) = std::fs::canonicalize(dir) else {
        return uploaded_manifest(state, os, arch, sha256);
    };
    let read = |name: &str| -> Option<String> {
        let path = std::fs::canonicalize(dir.join(format!("{os}-{arch}")).join(name)).ok()?;
        let metadata = std::fs::metadata(&path).ok()?;
        if !path.starts_with(&allowed)
            || !metadata.is_file()
            || metadata.len() > MAX_SIGNED_MANIFEST_BYTES
        {
            return None;
        }
        std::fs::read_to_string(path).ok()
    };
    let (manifest, signature) = match (
        read("poundlock.manifest.json"),
        read("poundlock.manifest.sig"),
    ) {
        (Some(manifest), Some(signature)) => (manifest, signature.trim().to_owned()),
        _ => return uploaded_manifest(state, os, arch, sha256),
    };
    let claims: Value = serde_json::from_str(&manifest).ok()?;
    if claims["os"] != os || claims["arch"] != arch || claims["sha256"] != sha256 {
        tracing::warn!(%os, %arch, "signed node manifest does not match the served binary; not offered");
        return None;
    }
    Some(SignedManifest {
        manifest,
        signature,
    })
}

/// A signature the release signer uploaded for exactly this binary.
fn uploaded_manifest(
    state: &AppState,
    os: &str,
    arch: &str,
    sha256: &str,
) -> Option<SignedManifest> {
    let (manifest, signature) = state.store.node_signature(os, arch, sha256).ok()??;
    Some(SignedManifest {
        manifest,
        signature,
    })
}

fn node_platform(node: &Node) -> (String, String) {
    let os = string_field(&node.capabilities, "os").unwrap_or_else(|| node.platform.clone());
    let arch = normalize_arch(&string_field(&node.capabilities, "arch").unwrap_or_default());
    (os, arch)
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn normalize_arch(arch: &str) -> String {
    match arch.trim() {
        "amd64" => "x86_64".into(),
        "arm64" => "aarch64".into(),
        other => other.to_owned(),
    }
}

#[derive(Clone)]
struct CachedFile {
    path: PathBuf,
    sha256: String,
    size: u64,
}

fn controller_linux_amd64() -> Option<&'static CachedFile> {
    if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
        return None;
    }
    static CACHED: OnceLock<Option<CachedFile>> = OnceLock::new();
    CACHED
        .get_or_init(|| load_file(std::env::current_exe().ok()?))
        .as_ref()
}

fn load_file(path: PathBuf) -> Option<CachedFile> {
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let size = metadata.len();
    if size == 0 || size > MAX_ARTIFACT_BYTES {
        return None;
    }
    Some(CachedFile {
        sha256: crate::crypto::sha256_hex_file(&path).ok()?,
        path,
        size,
    })
}
