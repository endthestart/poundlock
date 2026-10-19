use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The statuses a job ends in; a job in one of them never runs again. One list
/// for the Rust checks and, through `terminal_job_statuses!`, the SQL.
pub const TERMINAL_JOB_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];

/// The same statuses as an SQL list, for `concat!` in queries.
#[macro_export]
macro_rules! terminal_job_statuses {
    () => {
        "('completed','failed','cancelled')"
    };
}

pub fn is_terminal_job_status(status: &str) -> bool {
    TERMINAL_JOB_STATUSES.contains(&status)
}

/// The uses a model profile can serve. `completion` is a different kind of
/// model (FIM) and never combines with the coding uses.
pub const MODEL_PROFILE_USES: [&str; 3] = ["completion", "durable", "interactive"];

fn workload_class_to_use(class: &str) -> Result<&'static str, &'static str> {
    match class {
        "interactive-coding" => Ok("interactive"),
        "durable-coding-agent" => Ok("durable"),
        "fast-code-completion" => Ok("completion"),
        _ => Err("unknown workload_class"),
    }
}

/// The legacy single-class view of a `uses` set, for API responses.
/// Priority: interactive, then durable, then completion. Completion never
/// combines with the coding uses, so this is unambiguous in practice.
pub fn primary_workload_class(uses: &BTreeSet<String>) -> &'static str {
    if uses.contains("interactive") {
        "interactive-coding"
    } else if uses.contains("durable") {
        "durable-coding-agent"
    } else {
        "fast-code-completion"
    }
}

fn normalize_uses(uses: &[String]) -> Result<BTreeSet<String>, &'static str> {
    if uses.is_empty() {
        return Err("uses must not be empty");
    }
    let set: BTreeSet<String> = uses.iter().cloned().collect();
    if set
        .iter()
        .any(|value| !MODEL_PROFILE_USES.contains(&value.as_str()))
    {
        return Err("unknown use");
    }
    validate_uses_combination(&set)?;
    Ok(set)
}

fn validate_uses_combination(uses: &BTreeSet<String>) -> Result<(), &'static str> {
    if uses.contains("completion") && (uses.contains("interactive") || uses.contains("durable")) {
        return Err("completion cannot be combined with interactive or durable uses");
    }
    Ok(())
}

/// Resolve the `uses` set for a create/patch request that may specify
/// `uses`, the legacy `workload_class`, or both. When both are given they
/// must agree: the legacy use must be a member of `uses`. Returns `Ok(None)`
/// when neither is supplied, which callers treat as "no change" on patch and
/// as a missing-field error on create.
pub fn resolve_uses(
    uses: Option<&Vec<String>>,
    workload_class: Option<&str>,
) -> Result<Option<BTreeSet<String>>, &'static str> {
    let legacy = workload_class.map(workload_class_to_use).transpose()?;
    Ok(match (uses, legacy) {
        (Some(uses), legacy) => {
            let set = normalize_uses(uses)?;
            if let Some(legacy) = legacy
                && !set.contains(legacy)
            {
                return Err("uses and workload_class disagree");
            }
            Some(set)
        }
        (None, Some(legacy)) => Some(BTreeSet::from([legacy.to_string()])),
        (None, None) => None,
    })
}

/// What a new model serves unless the operator says otherwise: agent jobs and
/// interactive requests.
pub const DEFAULT_MODEL_USES: [&str; 2] = ["durable", "interactive"];

/// Agent jobs (the Pi harness) need at least this much context.
pub const AGENT_MIN_CONTEXT_TOKENS: u64 = 8192;

/// The default output limit for a context: a quarter of it, at most 32,768.
/// The relay refuses requests above the advertised limit, so a default that is
/// too low is not harmless.
pub fn default_max_output_tokens(context_tokens: u64) -> u64 {
    (context_tokens / 4).clamp(1, 32_768)
}

pub const API_TOKEN_PERMISSIONS: [&str; 7] = [
    "models:read",
    "inference:invoke",
    "inference:priority",
    "external:invoke",
    "jobs:read",
    "jobs:submit",
    "jobs:cancel",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub permissions: Vec<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiTokenPrincipal {
    pub id: String,
    pub name: String,
    pub permissions: Vec<String>,
}

impl ApiTokenPrincipal {
    pub fn has_permission(&self, permission: &str) -> bool {
        self.permissions
            .iter()
            .any(|candidate| candidate == permission)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ManagedIdentityPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm_limit: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateApiToken {
    pub name: String,
    pub permissions: Vec<String>,
}

#[derive(Serialize)]
pub struct IssuedApiToken {
    #[serde(flatten)]
    pub details: ApiToken,
    pub token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key_set: bool,
    pub created_at: String,
    #[serde(default)]
    pub managed_identity: ManagedIdentityPolicy,
    /// Where an operator opens this service, e.g. through a reverse proxy, when
    /// that differs from `base_url` (which is how the controller reaches it).
    /// Used for links only; never for requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Where a model server sends prompts, declared once for every model on
    /// it: `unknown`, `operator_network` or `external`. Absent for a gateway,
    /// whose routes each declare their own when published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_destination: Option<String>,
    #[serde(flatten)]
    pub last_check: ResourceCheck,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EndpointSecret {
    #[serde(flatten)]
    pub endpoint: Endpoint,
    pub api_key: String,
}

#[derive(Deserialize, Serialize)]
pub struct CreateEndpoint {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub managed_identity: ManagedIdentityPolicy,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedLiteLlmKey {
    pub api_token_id: String,
    pub endpoint_id: String,
    pub virtual_key: String,
    pub virtual_key_hash: String,
    pub remote_key_id: Option<String>,
    pub policy_hash: String,
    pub status: String,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A model profile's thinking default: `model_default` sends nothing to the
/// upstream server (current behaviour), `off` asks the relay and any runner
/// that talks to the endpoint directly to disable reasoning by default.
pub const MODEL_PROFILE_THINKING_VALUES: [&str; 2] = ["model_default", "off"];

fn model_default_thinking() -> String {
    "model_default".into()
}

/// Capability names the Toolkit measures and Poundlock stores/publishes.
/// Each is named after the fixture that produces it, not the routing
/// decision it might inform. A name absent from a profile's `capabilities`
/// is unknown, never a measured failure and never treated as capable.
pub const MODEL_PROFILE_CAPABILITY_NAMES: [&str; 3] = [
    "code_repair_verified",
    "plan_tool_schema",
    "sealed_investigation",
];

/// `qualified` and `smoke` are both measured passes, differing only in how
/// much margin the run had. `insufficient` means the run did not establish
/// the capability — which is **not** the same as measuring it to fail: a run
/// where every attempt passed but identity or freshness could not be
/// confirmed is `insufficient` too. `describe_capability` publishes
/// `measured_failure` for the narrower thing a router must act on.
///
/// There is deliberately no "unknown" status: an unmeasured capability is
/// simply absent from the list.
pub const MODEL_PROFILE_CAPABILITY_STATUSES: [&str; 3] = ["qualified", "smoke", "insufficient"];

/// The only source required to carry reproducibility evidence (run ids and a
/// result hash). Other source strings may exist later for manual/operator
/// annotation without run provenance, so `source` is not a closed enum.
pub const CAPABILITY_SOURCE_TOOLKIT: &str = "poundlock_toolkit";

/// Mirrors the Toolkit's own freshness rule: evidence older than this is
/// shown as stale rather than trusted indefinitely.
pub const CAPABILITY_STALE_AFTER_DAYS: i64 = 30;

/// Bound on `CapabilityMeasurement::note`, matching the bound already used
/// for `model_label`-style free text fields on this struct.
pub const CAPABILITY_NOTE_MAX_BYTES: usize = 2048;

/// The configuration a capability measurement was taken under. Compared
/// against a profile's live `context_tokens`/`thinking` at read time
/// (`capability_settings_match`) — a measurement made at a different context
/// window or reasoning default describes a different configuration, not
/// necessarily this one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeasuredSettings {
    pub context_tokens: u64,
    pub max_output_tokens: u64,
    /// `model_default` or `off`, matching `MODEL_PROFILE_THINKING_VALUES`.
    pub reasoning: String,
    pub harness: String,
}

/// One measured capability, pasted from `scripts/toolkit-capabilities.py`
/// output. Evidence is measured, never inferred: an entry
/// exists only because a scored run produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityMeasurement {
    /// One of `MODEL_PROFILE_CAPABILITY_NAMES`.
    pub name: String,
    /// One of `MODEL_PROFILE_CAPABILITY_STATUSES`.
    pub status: String,
    pub passed: u32,
    pub attempted: u32,
    /// `poundlock_toolkit` for a block made from Toolkit results; see
    /// `CAPABILITY_SOURCE_TOOLKIT`.
    pub source: String,
    #[serde(default)]
    pub toolkit_version: String,
    #[serde(default)]
    pub run_ids: Vec<String>,
    #[serde(default)]
    pub identity_sha256: String,
    #[serde(default)]
    pub result_sha256: String,
    /// RFC 3339. When the Toolkit measured this, not when Poundlock recorded it.
    pub measured_at: String,
    pub measured_settings: MeasuredSettings,
    #[serde(default)]
    pub note: String,
    /// Set by Poundlock when the entry is stored; any client-supplied value
    /// is overwritten, the same way other `*_at` bookkeeping fields are
    /// server-controlled.
    #[serde(default)]
    pub recorded_at: String,
}

/// True when `measured` describes a profile's current live configuration.
/// Only `context_tokens` and reasoning are routing-relevant settings that can
/// silently drift out from under a stored measurement; comparing them is
/// also what decides whether patching `context_tokens`/`thinking` invalidates
/// a stored measurement (see `store/configuration.rs`).
pub fn capability_settings_match(
    profile_context_tokens: u64,
    profile_thinking: &str,
    measured: &MeasuredSettings,
) -> bool {
    let context_matches = measured.context_tokens == profile_context_tokens;
    // Only "off" is a distinct declared behaviour; any other value name
    // (today just "model_default") matches any non-"off" profile default.
    let reasoning_matches = (measured.reasoning == "off") == (profile_thinking == "off");
    context_matches && reasoning_matches
}

/// The read-time published view of one measurement: its stored fields plus
/// `age_days`, `stale` and `settings_match`, which are derived fresh on
/// every read and never stored.
pub fn describe_capability(capability: &CapabilityMeasurement, profile: &ModelProfile) -> Value {
    let mut value = serde_json::to_value(capability).unwrap_or_else(|_| json!({}));
    let age_days = capability_age_days(&capability.measured_at);
    let matches = capability_settings_match(
        profile.context_tokens,
        &profile.thinking,
        &capability.measured_settings,
    );
    value["age_days"] = json!(age_days);
    value["stale"] = json!(age_days > CAPABILITY_STALE_AFTER_DAYS);
    value["settings_match"] = json!(if matches { "exact" } else { "different" });
    // The one boolean a router should act on, derived here so every client
    // reads the same answer. `status` alone does not carry it: `insufficient`
    // covers both "it failed" and "we could not tell", and a client that
    // treated those alike would drop a working profile from routing.
    value["measured_failure"] = json!(capability.attempted > 0 && capability.passed == 0);
    value
}

/// The published `capabilities` array for a profile: every stored
/// measurement, augmented with its derived freshness/settings fields.
pub fn describe_capabilities(profile: &ModelProfile) -> Vec<Value> {
    profile
        .capabilities
        .iter()
        .map(|capability| describe_capability(capability, profile))
        .collect()
}

/// Days between `measured_at` and now. An unparseable timestamp (should not
/// happen past validation) is reported maximally stale rather than fresh,
/// since evidence is measured, never assumed — and so is a timestamp in the
/// future, which flooring at zero used to report as freshly measured
/// forever.
fn capability_age_days(measured_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(measured_at)
        .map(|parsed| {
            let days = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_days();
            if days < 0 { i64::MAX } else { days }
        })
        .unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub endpoint_id: String,
    pub model_id: String,
    /// Operator-declared identity for display; never used for routing.
    #[serde(default)]
    pub model_label: Option<String>,
    pub context_tokens: u64,
    pub max_output_tokens: u64,
    /// The uses this profile serves: any of interactive, durable, completion.
    pub uses: BTreeSet<String>,
    /// Primary use derived from `uses`, kept for API compatibility. Read-only.
    pub workload_class: String,
    pub wake_policy: String,
    pub node_id: Option<String>,
    pub runtime_profile: Option<String>,
    #[serde(default)]
    pub idle_seconds: Option<u64>,
    /// Where this model's prompts go, whichever declaration it comes from.
    #[serde(default = "unknown_destination")]
    pub data_destination: String,
    /// `endpoint` when the model inherits its server's declaration; `profile`
    /// when it carries its own, as every gateway route does.
    #[serde(default = "profile_destination_source")]
    pub data_destination_source: String,
    /// `model_default` or `off`. See `MODEL_PROFILE_THINKING_VALUES`.
    #[serde(default = "model_default_thinking")]
    pub thinking: String,
    /// Reasoning-token budget applied when a llama.cpp request turns thinking on
    /// and names none of its own. Only for profiles on the `llamacpp` runtime
    /// (see `serves_llamacpp`), and never above `max_output_tokens`.
    #[serde(default)]
    pub thinking_budget_tokens: Option<u64>,
    /// Measured evidence pasted from the Toolkit. Empty means unknown — never
    /// treated as capable. See `MODEL_PROFILE_CAPABILITY_NAMES`.
    #[serde(default)]
    pub capabilities: Vec<CapabilityMeasurement>,
    /// What serves this profile and what it accepts, as published to clients.
    #[serde(default)]
    pub catalog: RouteCatalog,
    /// Whether authoritative evidence has taken this profile out of service.
    #[serde(default)]
    pub retirement: Retirement,
    pub created_at: String,
    #[serde(flatten)]
    pub last_check: ResourceCheck,
}

/// The node runtime name the llama.cpp manager (`node-runtimes/llamacpp`) is
/// configured under, `[runtimes.llamacpp]`. The node reports no engine kind, so
/// the name is what says a profile is served by llama-server.
pub const LLAMACPP_RUNTIME: &str = "llamacpp";

/// Whether llama.cpp answers this profile, and so honours
/// `chat_template_kwargs` and `thinking_budget_tokens` but not a top-level
/// `reasoning_effort`.
pub fn serves_llamacpp(profile: &ModelProfile) -> bool {
    profile.runtime_profile.as_deref() == Some(LLAMACPP_RUNTIME)
}

/// Explicit reasoning values a client may send, in the OpenAI-compatible
/// `reasoning_effort` vocabulary. `max` is deliberately absent: LiteLLM's
/// OpenRouter adapter rewrites it to `xhigh`, and a setting that is silently
/// changed on the way is worse than one that is refused.
pub const REASONING_LEVELS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

/// Where a published price comes from. It is an estimate either way; the bill
/// is whatever the gateway records.
pub const PRICING_SOURCES: [&str; 5] = [
    "litellm_route",
    "litellm_cost_map",
    "openrouter_catalog",
    "local",
    "operator",
];

/// What Poundlock publishes about a profile's route beyond how it is reached.
/// None of it is used for routing; all of it is shown to people and clients.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RouteCatalog {
    /// Explicit reasoning values shown by a live request to take effect on this
    /// route. Empty means none are verified: clients are offered only the
    /// model's default, and an explicit value is passed through unchecked, as
    /// before this field existed. Non-empty means anything else is refused.
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_verified_at: Option<String>,
    /// The model that actually answers, as the gateway names it — for example
    /// `openrouter/openai/gpt-6-sol` behind the stable name `frontier-gpt-sol`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    /// Who serves it: `local`, `openrouter`, `anthropic`, … Display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
}

/// An estimated price, never a bill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// One of `PRICING_SOURCES`.
    pub source: String,
    pub refreshed_at: String,
}

/// Evidence that a profile's upstream model is gone, and whether it has been
/// taken out of service for it. Only authoritative answers count — a model or
/// route reported as not found. Timeouts, rate limits, server errors, a busy
/// slot or a sleeping machine never do.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Retirement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<DisabledState>,
    /// Consecutive authoritative failures since the last success.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hard_failures: Vec<HardFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HardFailure {
    pub at: String,
    pub status: u16,
    /// `relay`, `job`, `check` or `discovery`.
    pub source: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisabledState {
    pub reason: String,
    pub source: String,
    pub since: String,
    #[serde(default)]
    pub evidence: Vec<HardFailure>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateModelProfile {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub endpoint_id: String,
    pub model_id: String,
    /// Operator-declared identity for display; never used for routing.
    #[serde(default)]
    pub model_label: Option<String>,
    pub context_tokens: u64,
    pub max_output_tokens: u64,
    /// Preferred way to declare a profile's uses.
    #[serde(default)]
    pub uses: Option<Vec<String>>,
    /// Legacy single-use field, still accepted. Must agree with `uses` when
    /// both are given; at least one of the two is required.
    #[serde(default)]
    pub workload_class: Option<String>,
    pub wake_policy: String,
    pub node_id: Option<String>,
    pub runtime_profile: Option<String>,
    #[serde(default)]
    pub idle_seconds: Option<u64>,
    /// On a gateway, this route's own declaration. On a model server the
    /// server declares it; `unknown` or omitted inherits it, and the same value
    /// is accepted. A server nobody declared yet takes the first model's.
    #[serde(default = "unknown_destination")]
    pub data_destination: String,
    /// `model_default` or `off`. See `MODEL_PROFILE_THINKING_VALUES`.
    #[serde(default = "model_default_thinking")]
    pub thinking: String,
    /// See `ModelProfile::thinking_budget_tokens`.
    #[serde(default)]
    pub thinking_budget_tokens: Option<u64>,
    /// Measured evidence pasted from the Toolkit. Usually omitted on
    /// creation. See `MODEL_PROFILE_CAPABILITY_NAMES`.
    #[serde(default)]
    pub capabilities: Vec<CapabilityMeasurement>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PatchModelProfile {
    pub name: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
    pub endpoint_id: Option<String>,
    pub model_id: Option<String>,
    /// Omit to preserve, null to clear the operator-declared identity.
    #[serde(default, deserialize_with = "optional_nullable")]
    pub model_label: Option<Option<String>>,
    pub context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub uses: Option<Vec<String>>,
    pub workload_class: Option<String>,
    pub wake_policy: Option<String>,
    /// Omit to leave unchanged. Empty string detaches the node.
    pub node_id: Option<String>,
    /// Omit to leave unchanged. Empty string detaches the runtime profile.
    pub runtime_profile: Option<String>,
    #[serde(default, deserialize_with = "optional_nullable")]
    pub idle_seconds: Option<Option<u64>>,
    pub data_destination: Option<String>,
    /// Omit to leave unchanged. `model_default` or `off`.
    pub thinking: Option<String>,
    /// Omit to leave unchanged, null to clear. llama.cpp profiles only.
    #[serde(default, deserialize_with = "optional_nullable")]
    pub thinking_budget_tokens: Option<Option<u64>>,
    /// Omit to leave unchanged. Replaces the entire list — this is a paste
    /// of the Toolkit's block, not a per-entry edit. An explicit empty array
    /// clears all measurements.
    #[serde(default)]
    pub capabilities: Option<Vec<CapabilityMeasurement>>,
    /// Omit to leave unchanged. Replaces the whole published catalog entry.
    #[serde(default)]
    pub catalog: Option<RouteCatalog>,
}

fn unknown_destination() -> String {
    "unknown".into()
}

fn profile_destination_source() -> String {
    "profile".into()
}

/// The declarations a destination can hold.
pub const DATA_DESTINATIONS: [&str; 3] = ["unknown", "operator_network", "external"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessProfile {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub version: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateHarnessProfile {
    pub name: String,
    pub kind: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub roots: Vec<String>,
    #[serde(default)]
    pub instruction_files: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateProject {
    pub name: String,
    pub roots: Vec<String>,
    #[serde(default)]
    pub instruction_files: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PatchProject {
    pub name: Option<String>,
    pub roots: Option<Vec<String>>,
    #[serde(default)]
    pub instruction_files: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub idempotency_key: String,
    pub project_id: Option<String>,
    pub model_profile_id: String,
    pub harness_profile_id: String,
    pub prompt: String,
    #[serde(default)]
    pub policy: crate::job_policy::JobPolicy,
    pub status: String,
    pub attempt_id: String,
    pub cancel_requested: bool,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubmitJob {
    pub idempotency_key: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::source::JobSource>,
    pub model_profile_id: String,
    pub harness_profile_id: String,
    pub prompt: String,
    #[serde(
        default,
        skip_serializing_if = "crate::job_policy::JobPolicy::is_default"
    )]
    pub policy: crate::job_policy::JobPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: i64,
    pub job_id: String,
    pub kind: String,
    pub message: String,
    pub payload: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub capabilities: Value,
    pub status: String,
    pub last_seen_at: Option<String>,
    pub wol_mac: Option<String>,
    pub wol_broadcast: Option<String>,
    pub created_at: String,
}

impl Node {
    pub fn network_problem(&self) -> Option<String> {
        let network = self.capabilities.get("network")?;
        if network.get("mode").and_then(Value::as_str) != Some("interface") {
            return None;
        }
        let interface = network.get("interface")?.as_str()?;
        (network.get("status").and_then(Value::as_str) != Some("connected"))
            .then(|| format!("Selected network interface '{interface}' is unavailable. Check the node's connection and network settings."))
    }
}

#[derive(Deserialize, Serialize)]
pub struct EnrollNode {
    pub token: String,
    pub name: String,
    pub platform: String,
    pub capabilities: Value,
}

#[derive(Serialize, Deserialize)]
pub struct EnrolledNode {
    pub node_id: String,
    pub credential: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAction {
    pub id: String,
    pub node_id: String,
    pub job_id: Option<String>,
    pub kind: String,
    pub payload: Value,
    pub status: String,
    pub result: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootPatch {
    pub root: String,
    pub base_revision: String,
    pub patch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerPackage {
    pub job: Job,
    pub project: Project,
    #[serde(default)]
    pub source: Option<Value>,
    pub model: ModelProfile,
    pub endpoint: EndpointSecret,
    pub harness: HarnessProfile,
    /// Resolved serving identity for provenance (model label plus the node's
    /// reported application/build/commit/model for this model's runtime),
    /// filled in at claim time from data the runner itself cannot see. Never
    /// set at contract-freeze time; absent when nothing is known.
    #[serde(default)]
    pub serving_identity: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RunnerResult {
    pub patches: Vec<RootPatch>,
    pub output: String,
    pub provenance: Value,
    pub cleanup: Value,
}

fn optional_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

// Hand-written so a debug print or a logged error never carries the secret (L7).
const REDACTED: &str = "<redacted>";

impl std::fmt::Debug for IssuedApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedApiToken")
            .field("details", &self.details)
            .field("token", &REDACTED)
            .finish()
    }
}

impl std::fmt::Debug for EndpointSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointSecret")
            .field("endpoint", &self.endpoint)
            .field("api_key", &REDACTED)
            .finish()
    }
}

impl std::fmt::Debug for CreateEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateEndpoint")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &REDACTED)
            .field("managed_identity", &self.managed_identity)
            .finish()
    }
}

impl std::fmt::Debug for EnrollNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollNode")
            .field("token", &REDACTED)
            .field("name", &self.name)
            .field("platform", &self.platform)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl std::fmt::Debug for EnrolledNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolledNode")
            .field("node_id", &self.node_id)
            .field("credential", &REDACTED)
            .finish()
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_output_never_carries_a_secret() {
        let printed = [
            format!(
                "{:?}",
                CreateEndpoint {
                    name: "gateway".into(),
                    kind: "openai".into(),
                    base_url: "https://gateway.invalid".into(),
                    api_key: "sk-endpoint-secret".into(),
                    managed_identity: ManagedIdentityPolicy::default(),
                }
            ),
            format!(
                "{:?}",
                EnrollNode {
                    token: "enroll-secret".into(),
                    name: "orion".into(),
                    platform: "linux".into(),
                    capabilities: Value::Null,
                }
            ),
            format!(
                "{:?}",
                EnrolledNode {
                    node_id: "node-1".into(),
                    credential: "node-secret".into(),
                }
            ),
        ];
        for line in &printed {
            assert!(line.contains("<redacted>"), "{line}");
            assert!(!line.contains("secret\""), "{line}");
        }
        assert!(printed[0].contains("gateway.invalid") && printed[2].contains("node-1"));
    }
}

#[cfg(test)]
mod terminal_status_tests {
    #[test]
    fn the_sql_list_names_the_same_statuses() {
        let sql = crate::terminal_job_statuses!();
        let listed: Vec<&str> = sql
            .trim_matches(|c| c == '(' || c == ')')
            .split(',')
            .map(|status| status.trim_matches('\''))
            .collect();
        assert_eq!(listed, super::TERMINAL_JOB_STATUSES);
    }
}
