use serde_json::{Value, json};

pub const API_MAJOR: u32 = 1;
pub const CONTRACT_REVISION: u32 = 5;
pub const SCHEMA_VERSION: u32 = 1;

pub const FEATURE_ACCESS_PREFLIGHT: &str = "access_preflight";
pub const FEATURE_AVAILABILITY_REASONS: &str = "availability_reasons";
/// `poundlock.capabilities` on `/v1/models` and job-options rows: measured
/// capability evidence pasted from the Toolkit, added in revision 3.
pub const FEATURE_CAPABILITY_EVIDENCE: &str = "capability_evidence";
/// A request for a busy model waits in line (`Poundlock-Wait`, `-Priority`,
/// `-Session`, `-Queue-Ticket`) instead of an immediate 409, and
/// `GET /v1/capacity` shows the lines (revision 5).
pub const FEATURE_CAPACITY_QUEUE: &str = "capacity_queue";
pub const FEATURE_DATA_DESTINATION: &str = "data_destination";
pub const FEATURE_DURABLE_JOBS: &str = "durable_jobs";
pub const FEATURE_EXTERNAL_INVOKE: &str = "external_invoke_permission";
pub const FEATURE_MANAGED_IDENTITIES: &str = "managed_application_identities";
/// `poundlock.status` and `poundlock.disabled`: a profile taken out of
/// service on authoritative evidence stays listed, with why (revision 4).
pub const FEATURE_MODEL_STATUS: &str = "model_status";
/// `poundlock.reasoning`: the explicit reasoning levels shown to take effect;
/// others are refused with `unsupported_reasoning_effort` (revision 4).
pub const FEATURE_REASONING_LEVELS: &str = "reasoning_levels";
/// `poundlock.served_model`, `provider` and `pricing` with its source:
/// what answers behind a stable name, for display only (revision 4).
pub const FEATURE_ROUTE_CATALOG: &str = "route_catalog";
pub const FEATURE_ROUTING_REVISION: &str = "routing_revision";
pub const FEATURE_USAGE_SUMMARY: &str = "usage_summary";
pub const FEATURE_VERIFIER_TOOLCHAIN: &str = "verifier_toolchain";

pub const CAPABILITY_FEATURES: [&str; 14] = [
    FEATURE_ACCESS_PREFLIGHT,
    FEATURE_AVAILABILITY_REASONS,
    FEATURE_CAPABILITY_EVIDENCE,
    FEATURE_CAPACITY_QUEUE,
    FEATURE_DATA_DESTINATION,
    FEATURE_DURABLE_JOBS,
    FEATURE_EXTERNAL_INVOKE,
    FEATURE_MANAGED_IDENTITIES,
    FEATURE_MODEL_STATUS,
    FEATURE_REASONING_LEVELS,
    FEATURE_ROUTE_CATALOG,
    FEATURE_ROUTING_REVISION,
    FEATURE_USAGE_SUMMARY,
    FEATURE_VERIFIER_TOOLCHAIN,
];

pub fn capabilities_document(product_version: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "api_major": API_MAJOR,
        "contract_revision": CONTRACT_REVISION,
        "product_version": product_version,
        "features": CAPABILITY_FEATURES,
        "verifier": crate::verifier_toolchain::advertised(),
    })
}

pub fn models_poundlock_metadata() -> Value {
    json!({
        "contract_revision": CONTRACT_REVISION,
        "schema_version": SCHEMA_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAPI: &str = include_str!("../docs/reference/openapi.yaml");
    const DOCKERFILE: &str = include_str!("../Dockerfile");

    #[test]
    fn openapi_metadata_matches_the_runtime_contract() {
        assert!(
            OPENAPI.contains(&format!(
                "\n  x-poundlock-contract-revision: {CONTRACT_REVISION}\n"
            )),
            "OpenAPI info must publish contract revision {CONTRACT_REVISION}"
        );
        assert!(OPENAPI.contains(&format!("\n  x-poundlock-api-major: {API_MAJOR}\n")));
        assert!(OPENAPI.contains(&format!("\n  version: {}\n", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn openapi_component_references_resolve() {
        for reference in OPENAPI.split("$ref: \"#/components/").skip(1) {
            let target = reference.split('"').next().unwrap();
            let (kind, name) = target.split_once('/').unwrap();
            let defined = OPENAPI
                .lines()
                .skip_while(|line| *line != format!("  {kind}:"))
                .skip(1)
                .take_while(|line| line.is_empty() || line.starts_with("   "))
                .any(|line| line == format!("    {name}:"));
            assert!(defined, "unresolved OpenAPI reference components/{target}");
        }
    }

    #[test]
    fn published_fixtures_match_the_runtime_contract() {
        let models: Value = serde_json::from_str(include_str!(
            "../docs/reference/fixtures/models-contract-revision-5.json"
        ))
        .unwrap();
        assert_eq!(models["poundlock"], models_poundlock_metadata());
        let capabilities: Value = serde_json::from_str(include_str!(
            "../docs/reference/fixtures/capabilities-contract-revision-5.json"
        ))
        .unwrap();
        assert_eq!(
            capabilities,
            capabilities_document(env!("CARGO_PKG_VERSION"))
        );
    }

    /// The capability contract, which `scripts/toolkit-capabilities.py`
    /// validates its output against. Before it existed the producer emitted a
    /// shape this struct rejects — no `source`, a list where a string was
    /// required — and the only way to find out was to paste a block and read
    /// the 400.
    #[test]
    fn the_published_capability_schema_is_the_shape_this_struct_accepts() {
        use crate::domain::{
            CapabilityMeasurement, MODEL_PROFILE_CAPABILITY_NAMES,
            MODEL_PROFILE_CAPABILITY_STATUSES,
        };
        let schema: Value = serde_json::from_str(include_str!(
            "../docs/reference/fixtures/capability-measurement.schema.json"
        ))
        .unwrap();
        let properties = schema["properties"].as_object().unwrap();

        // The enums are the Rust constants, not a second list to keep in step.
        assert_eq!(
            properties["name"]["enum"],
            json!(MODEL_PROFILE_CAPABILITY_NAMES)
        );
        assert_eq!(
            properties["status"]["enum"],
            json!(MODEL_PROFILE_CAPABILITY_STATUSES)
        );

        let example = json!({
            "name": "sealed_investigation",
            "status": "qualified",
            "passed": 3,
            "attempted": 3,
            "source": crate::domain::CAPABILITY_SOURCE_TOOLKIT,
            "toolkit_version": "0.1.0",
            "run_ids": ["run-a", "run-b"],
            "identity_sha256": "a".repeat(64),
            "result_sha256": "b".repeat(64),
            "measured_at": "2026-09-20T00:00:00Z",
            "measured_settings": {
                "context_tokens": 32768,
                "max_output_tokens": 4096,
                "reasoning": "model_default",
                "harness": "pi"
            }
        });
        let parsed: CapabilityMeasurement = serde_json::from_value(example.clone()).unwrap();
        assert_eq!(parsed.source, crate::domain::CAPABILITY_SOURCE_TOOLKIT);

        // Every key the struct can hold is described, and nothing more: an
        // undescribed field is one the Toolkit would never learn to send.
        let described: std::collections::BTreeSet<&str> =
            properties.keys().map(String::as_str).collect();
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        for field in round_tripped.as_object().unwrap().keys() {
            assert!(
                described.contains(field.as_str()),
                "undescribed field {field}"
            );
        }

        // Every key the schema calls required really is: dropping it must
        // stop the paste, either at deserialization or at validation.
        for required in schema["required"].as_array().unwrap() {
            let field = required.as_str().unwrap();
            let mut without = example.clone();
            without.as_object_mut().unwrap().remove(field);
            let rejected = match serde_json::from_value::<CapabilityMeasurement>(without) {
                Err(_) => true,
                Ok(value) => crate::store::configuration::validate_capabilities(&[value]).is_some(),
            };
            assert!(
                rejected,
                "schema calls {field} required but the struct accepts it missing"
            );
        }
    }

    #[test]
    fn advertised_verifier_toolchain_matches_the_image_build() {
        use crate::verifier_toolchain::{PI_VERSION, PYTHON_SERIES, UV_VERSION};
        assert!(DOCKERFILE.contains(&format!("ghcr.io/astral-sh/uv:{UV_VERSION}@sha256:")));
        assert!(DOCKERFILE.contains(&format!("uv python install {PYTHON_SERIES} ")));
        assert!(DOCKERFILE.contains(&format!("ARG PI_VERSION={PI_VERSION}\n")));
    }
}
