use super::{ApiError, ApiResult, AppState, admin_auth, bearer};
use crate::domain::{EnrollNode, EnrolledNode, Node, NodeAction};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/nodes", get(list_nodes))
        .route("/api/v1/nodes/{id}", delete(remove_node))
        .route("/api/v1/nodes/{id}/maintenance", post(admin_maintenance))
        .route("/api/v1/nodes/enrollment-tokens", post(create_enrollment))
        .route("/api/v1/nodes/config-example", get(config_example))
        .route("/api/v1/nodes/linux-installer", get(linux_installer))
        .route("/api/v1/nodes/{id}/wol", put(configure_wol))
        .route("/api/v1/nodes/{id}/wake-test", post(wake_test))
        .route("/node/v1/enroll", post(enroll))
        .route("/node/v1/nodes/{id}/status", get(node_status))
        .route("/node/v1/nodes/{id}/revoke", post(revoke_node))
        .route("/node/v1/nodes/{id}/maintenance", post(maintenance))
        .route("/node/v1/nodes/{id}/heartbeat", post(heartbeat))
        .route("/node/v1/nodes/{id}/actions/poll", post(poll_action))
        .route(
            "/node/v1/nodes/{id}/actions/{action_id}/complete",
            post(complete_action),
        )
}

async fn list_nodes(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Node>>> {
    admin_auth(&state, &headers)?;
    Ok(Json(
        state.store.list_nodes(state.config.node_offline_seconds)?,
    ))
}

async fn create_enrollment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<EnrollmentRequest>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let minutes = input.valid_minutes.unwrap_or(15).clamp(1, 1440);
    let name = input
        .node_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if name.is_some_and(|name| name.len() > 80) {
        return Err(ApiError::bad("node name must be 1 to 80 characters"));
    }
    Ok(Json(json!({
        "token": state.store.create_enrollment_token(minutes, name)?,
        "valid_minutes": minutes,
        "node_name": name,
    })))
}

async fn remove_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    if !state
        .occupancy
        .with_idle_node(&id, || state.store.remove_node(&id))
        .map_err(|error| ApiError::conflict(error.to_string()))?
    {
        return Err(ApiError::not_found("machine not found"));
    }
    Ok(Json(json!({"removed":true})))
}

async fn configure_wol(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<WolRequest>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    if let Some(mac) = &input.mac {
        crate::wake::parse_mac(mac).map_err(ApiError::bad)?;
        // A machine with two interfaces is easy to mis-wire: waking one NIC
        // while inference is configured on the other fails silently later.
        if let Some(node) = state.store.node(&id, state.config.node_offline_seconds)?
            && let Some(reported) = reported_macs(&node)
            && !reported.iter().any(|known| known.eq_ignore_ascii_case(mac))
        {
            return Err(ApiError::bad(format!(
                "this machine reports {}, not {mac}",
                reported.join(", ")
            )));
        }
    }
    if !state
        .store
        .configure_wol(&id, input.mac.as_deref(), input.broadcast.as_deref())?
    {
        return Err(ApiError::not_found("node not found"));
    }
    Ok(Json(json!({"configured":true})))
}

/// Interface MAC addresses the machine itself reported, when it has reported any.
fn reported_macs(node: &crate::domain::Node) -> Option<Vec<String>> {
    let interfaces = node
        .capabilities
        .get("network")?
        .get("interfaces")?
        .as_array()?;
    let macs: Vec<String> = interfaces
        .iter()
        .filter_map(|item| item.get("mac")?.as_str())
        .filter(|mac| !mac.is_empty())
        .map(str::to_owned)
        .collect();
    (!macs.is_empty()).then_some(macs)
}

async fn config_example(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    Ok(Json(json!({
        "name":"node.toml",
        "version":env!("CARGO_PKG_VERSION"),
        "toml":crate::node::example_config()
    })))
}

async fn wake_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<WakeTestRequest>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let node = state
        .store
        .node(&id, state.config.node_offline_seconds)?
        .ok_or_else(|| ApiError::not_found("node not found"))?;
    let mac = node
        .wol_mac
        .as_deref()
        .ok_or_else(|| ApiError::bad("Wake-on-LAN is not configured for this machine"))?;
    let broadcast = node.wol_broadcast.as_deref().unwrap_or("255.255.255.255:9");
    let was_online = node.status == "online";
    crate::wake::request_magic_packet(state.config.wake_socket.as_deref(), mac, broadcast)
        .await
        .map_err(|error| ApiError::unavailable(error.to_string()))?;
    let started = chrono::Utc::now();
    let timeout = input.timeout_seconds.unwrap_or(60).clamp(1, 120);
    let deadline = started + chrono::Duration::seconds(timeout as i64);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let node = state
            .store
            .node(&id, state.config.node_offline_seconds)?
            .ok_or_else(|| ApiError::not_found("node not found"))?;
        let seen = node
            .last_seen_at
            .as_deref()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|value| value.with_timezone(&chrono::Utc));
        if seen.is_some_and(|value| value > started) {
            return Ok(Json(json!({
                "awake":true,
                "was_online":was_online,
                "packet_sent":true,
                "last_seen_at":node.last_seen_at
            })));
        }
        if chrono::Utc::now() >= deadline {
            return Err(ApiError::unavailable(
                "No heartbeat after Wake-on-LAN. The machine may be powered off or unable to reach this controller.",
            ));
        }
    }
}

async fn enroll(
    State(state): State<AppState>,
    Json(mut input): Json<EnrollNode>,
) -> ApiResult<Json<EnrolledNode>> {
    input.name = input.name.trim().to_owned();
    if input.name.is_empty() || input.name.len() > 80 {
        return Err(ApiError::bad("node name must be 1 to 80 characters"));
    }
    input.platform = input.platform.trim().to_owned();
    if input.platform.is_empty() || input.platform.len() > 40 {
        return Err(ApiError::bad("node platform must be 1 to 40 characters"));
    }
    let enrolled = state
        .store
        .enroll_node_with_offline(&input, state.config.node_offline_seconds)
        .map_err(|error| {
            tracing::warn!(node = %input.name, %error, "node enrollment failed");
            if error.to_string() == crate::store::LIVE_ENROLLMENT_CONFLICT {
                ApiError::conflict(crate::store::LIVE_ENROLLMENT_CONFLICT)
            } else {
                ApiError::bad("node enrollment failed; verify the name and single-use token")
            }
        })?;
    Ok(Json(enrolled))
}

async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Heartbeat>,
) -> ApiResult<Json<Value>> {
    node_auth(&state, &headers, &id)?;
    state
        .store
        .heartbeat_node(&id, &input.platform, &merge_identity(&input))?;
    Ok(Json(json!({"accepted":true})))
}

/// Hands the node its next action. A node that sends `wait_seconds` has an
/// empty poll held open for up to that long, capped, and is answered the moment
/// an action is queued; without it the poll answers at once, as older agents
/// expect. The body is read leniently for the same reason: anything that is not
/// a `wait_seconds` request is an immediate poll.
async fn poll_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Option<NodeAction>>> {
    node_auth(&state, &headers, &id)?;
    let wait = serde_json::from_slice::<PollRequest>(&body)
        .unwrap_or_default()
        .wait_seconds
        .min(crate::node::MAX_POLL_WAIT_SECONDS);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);
    let signal = state.store.node_signal(&id);
    loop {
        // Subscribe before looking, so an action queued in between still wakes us.
        let queued = signal.queued.notified();
        if let Some(action) = state.store.poll_node_action(&id)? {
            return Ok(Json(Some(action)));
        }
        if tokio::time::timeout_at(deadline, queued).await.is_err() {
            return Ok(Json(None));
        }
        // The credentials may have been replaced or revoked while the poll was held.
        node_auth(&state, &headers, &id)?;
    }
}

async fn complete_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, action_id)): Path<(String, String)>,
    Json(input): Json<ActionResult>,
) -> ApiResult<Json<Value>> {
    node_auth(&state, &headers, &id)?;
    if !state
        .store
        .complete_node_action(&id, &action_id, input.success, input.result)?
    {
        return Err(ApiError::conflict("action is absent or no longer running"));
    }
    Ok(Json(json!({"accepted":true})))
}

pub(super) fn node_auth(state: &AppState, headers: &HeaderMap, id: &str) -> ApiResult<()> {
    let credential =
        bearer(headers).ok_or_else(|| ApiError::unauthorized("node bearer token required"))?;
    if state.store.authenticate_node(id, credential)? {
        Ok(())
    } else {
        Err(ApiError::unauthorized("node credentials are invalid"))
    }
}

#[derive(Deserialize)]
struct EnrollmentRequest {
    valid_minutes: Option<i64>,
    /// The machine this token may enroll. Required to re-enroll an existing one.
    node_name: Option<String>,
}
#[derive(Deserialize)]
struct WolRequest {
    mac: Option<String>,
    broadcast: Option<String>,
}
#[derive(Deserialize)]
struct WakeTestRequest {
    #[serde(default)]
    timeout_seconds: Option<u64>,
}
#[derive(Deserialize)]
struct Heartbeat {
    platform: String,
    capabilities: Value,
    #[serde(default)]
    agent_version: Option<String>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    arch: Option<String>,
}
#[derive(Default, Deserialize)]
struct PollRequest {
    #[serde(default)]
    wait_seconds: u64,
}
#[derive(Deserialize)]
struct ActionResult {
    success: bool,
    result: Value,
}

fn merge_identity(input: &Heartbeat) -> Value {
    let mut capabilities = if input.capabilities.is_object() {
        input.capabilities.clone()
    } else {
        json!({})
    };
    let Some(object) = capabilities.as_object_mut() else {
        return capabilities;
    };
    if let Some(version) = &input.agent_version {
        object.insert("agent_version".into(), json!(version));
    }
    object.insert(
        "os".into(),
        json!(input.os.clone().unwrap_or_else(|| input.platform.clone())),
    );
    if let Some(arch) = &input.arch {
        object.insert("arch".into(), json!(arch));
    }
    capabilities
}

async fn node_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    node_auth(&state, &headers, &id)?;
    let node = state
        .store
        .node(&id, state.config.node_offline_seconds)?
        .ok_or_else(|| ApiError::not_found("node not found"))?;
    Ok(Json(
        json!({"node_id":id,"name":node.name,"status":node.status,"last_seen_at":node.last_seen_at,"active":state.occupancy.snapshot().iter().any(|slot|slot.node_id==id) || state.store.node_active(&id)?}),
    ))
}
async fn revoke_node(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    node_auth(&state, &headers, &id)?;
    state
        .occupancy
        .with_idle_node(&id, || {
            state.store.revoke_node(
                &id,
                bearer(&headers)
                    .ok_or_else(|| ApiError::unauthorized("node credential required"))?,
            )
        })
        .map_err(|error| ApiError::bad(error.to_string()))?;
    Ok(Json(json!({"revoked":true})))
}

#[derive(Deserialize)]
struct MaintenanceRequest {
    paused: bool,
}
async fn admin_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<MaintenanceRequest>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    state
        .occupancy
        .with_idle_node(&id, || {
            state.store.set_node_maintenance(&id, None, input.paused)
        })
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    Ok(Json(json!({"paused":input.paused})))
}
async fn maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<MaintenanceRequest>,
) -> ApiResult<Json<Value>> {
    node_auth(&state, &headers, &id)?;
    let change = || {
        state
            .store
            .node_maintenance(&id, bearer(&headers).unwrap(), input.paused)
    };
    if input.paused {
        state.occupancy.with_idle_node(&id, change)
    } else {
        change()
    }
    .map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(json!({"paused":input.paused})))
}

async fn linux_installer(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<impl axum::response::IntoResponse> {
    admin_auth(&state, &headers)?;
    Ok((
        [
            ("content-type", "text/x-shellscript; charset=utf-8"),
            (
                "content-disposition",
                "attachment; filename=\"poundlock-node.sh\"",
            ),
        ],
        include_str!("../../deploy/linux/poundlock-node.sh"),
    ))
}
