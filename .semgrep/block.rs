// Positive and negative controls for block.yml. A rule that never fires would
// make the gate look clean, so CI runs `semgrep --test` here before the scan.
fn tls() {
    // ruleid: poundlock-rust-tls-verification-off
    let _a = reqwest::Client::builder().danger_accept_invalid_certs(true).build();
    // ruleid: poundlock-rust-tls-verification-off
    let _b = reqwest::Client::builder().danger_accept_invalid_hostnames(true).build();
    // ok: poundlock-rust-tls-verification-off
    let _c = reqwest::Client::builder().danger_accept_invalid_certs(false).build();
}
