use anyhow::{Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub(super) type Inhibitors = HashMap<String, LeaseInhibitor>;

pub(super) struct LeaseInhibitor {
    child: Child,
    renewed_at: Instant,
}

impl LeaseInhibitor {
    pub(super) async fn stop(&mut self) {
        self.kill_group();
        let _ = self.child.kill().await;
    }
    fn kill_group(&self) {
        #[cfg(unix)]
        if let Some(id) = self.child.id() {
            // The process group is created exclusively for this inhibitor.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
    }
}
impl Drop for LeaseInhibitor {
    fn drop(&mut self) {
        self.kill_group();
    }
}

pub(super) async fn acquire(lease: &str) -> Result<Option<LeaseInhibitor>> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("/usr/bin/caffeinate");
        command.args(["-dimsu"]);
        command
    } else if cfg!(target_os = "linux") {
        let mut command = Command::new("systemd-inhibit");
        command.args([
            // Older systemd versions authorize combined masks as lid-switch
            // inhibition. Separate requests retain only the needed permissions.
            "--what=sleep",
            "--mode=block",
            "--who=poundlock",
            &format!("--why=Active poundlock lease {lease}"),
            "systemd-inhibit",
            "--what=idle",
            "--mode=block",
            "--who=poundlock",
            &format!("--why=Active poundlock lease {lease}"),
            "/bin/sh",
            "-c",
            "printf 'ready\\n'; exec sleep infinity",
        ]);
        command
    } else {
        return Ok(None);
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().context("Start sleep inhibitor")?;
    let mut held = LeaseInhibitor {
        child,
        renewed_at: Instant::now(),
    };
    if cfg!(target_os = "linux") {
        // This acknowledgement comes from inside the acquired inhibitor.
        let mut output = BufReader::new(held.child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line))
            .await
            .context("Sleep inhibitor timed out; check systemd-logind and polkit")??;
        anyhow::ensure!(
            line.trim() == "ready",
            "Sleep inhibition denied; run node-doctor and check the installed polkit rule"
        );
    } else {
        tokio::time::sleep(Duration::from_millis(100)).await;
        anyhow::ensure!(
            held.child.try_wait()?.is_none(),
            "Sleep inhibitor exited during startup"
        );
    }
    Ok(Some(held))
}

/// An inhibitor around any child process, renewed just now, for upkeep tests.
#[cfg(test)]
pub(super) fn held(child: Child) -> LeaseInhibitor {
    LeaseInhibitor {
        child,
        renewed_at: Instant::now(),
    }
}

pub(super) fn renew(inhibitors: &mut Inhibitors, lease: &str) {
    if let Some(inhibitor) = inhibitors.get_mut(lease) {
        inhibitor.renewed_at = Instant::now();
    }
}

pub(super) async fn release(inhibitors: &mut Inhibitors, lease: &str) {
    if let Some(mut inhibitor) = inhibitors.remove(lease) {
        inhibitor.stop().await;
    }
}

pub(super) async fn expire(inhibitors: &mut Inhibitors, timeout: Duration) -> Vec<String> {
    let expired = inhibitors
        .iter()
        .filter(|(_, inhibitor)| inhibitor.renewed_at.elapsed() >= timeout)
        .map(|(lease, _)| lease.clone())
        .collect::<Vec<_>>();
    for lease in &expired {
        release(inhibitors, lease).await;
    }
    expired
}

pub(super) fn name() -> &'static str {
    if cfg!(target_os = "macos") {
        "caffeinate"
    } else if cfg!(target_os = "linux") {
        "systemd-inhibit"
    } else {
        "none"
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_inhibitors_are_expired() {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let mut inhibitors = Inhibitors::new();
        inhibitors.insert(
            "stale".into(),
            LeaseInhibitor {
                child,
                renewed_at: Instant::now() - Duration::from_secs(10),
            },
        );

        assert_eq!(
            expire(&mut inhibitors, Duration::from_secs(5)).await,
            ["stale"]
        );
        assert!(inhibitors.is_empty());
    }
}
