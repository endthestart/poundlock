// Positive and negative controls for report.yml.
use std::process::Command;

fn shells(script: &str) {
    // ruleid: poundlock-rust-shell-command
    let _a = Command::new("sh");
    // ruleid: poundlock-rust-shell-command
    let _b = std::process::Command::new("/bin/sh");
    // ruleid: poundlock-rust-shell-command
    let _c = tokio::process::Command::new("bash");
    // ok: poundlock-rust-shell-command
    let _d = Command::new("git");
    let _ = script;
}

// ruleid: poundlock-rust-debug-derive-on-secret
#[derive(Debug, Clone)]
struct Issued {
    name: String,
    token: String,
}

// ruleid: poundlock-rust-debug-derive-on-secret
#[derive(Debug)]
struct Endpoint {
    api_key: String,
}

// ok: poundlock-rust-debug-derive-on-secret
#[derive(Debug)]
struct Usage {
    context_tokens: u64,
    max_output_tokens: u64,
}

// ok: poundlock-rust-debug-derive-on-secret
#[derive(Clone)]
struct Quiet {
    token: String,
}

fn clients() {
    // ruleid: poundlock-rust-http-client-follows-redirects
    let _a = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build();
    // ruleid: poundlock-rust-http-client-follows-redirects
    let _b = reqwest::Client::new();
    // ok: poundlock-rust-http-client-follows-redirects
    let _c = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build();
}

struct Args {
    // ruleid: poundlock-rust-plain-http-default
    #[arg(long, default_value = "http://controller:7331")]
    remote: String,
    // ok: poundlock-rust-plain-http-default
    #[arg(long, default_value = "http://127.0.0.1:7331")]
    local: String,
}
