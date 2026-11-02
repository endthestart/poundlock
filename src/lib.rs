pub mod acp;
mod acp_local;
pub mod backup;
pub mod cli;
pub mod client;
pub mod config;
pub mod contract;
pub mod controller_client;
pub mod crypto;
pub mod domain;
pub mod git;
pub mod git_source;
pub mod harness;
pub mod job_policy;
mod litellm;
pub mod node;
mod process;
pub mod runner;
pub mod scheduler;
pub mod server;
pub mod source;
pub mod store;
mod tcp;
pub mod verifier_toolchain;
pub mod wake;

/// This build's version. A main build carries a sortable prerelease of the next
/// minor release, such as `0.4.0-dev.<commit time>`, set by CI, so nodes take every
/// main deploy: a node installs only a higher version.
/// A release, and a local build, use the package version.
pub const VERSION: &str = match option_env!("POUNDLOCK_BUILD_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};
