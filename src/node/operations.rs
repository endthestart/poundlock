use super::{NodeClient, NodeConfig, NodeCredential, NodeOptions};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

pub(super) fn load(mut options: NodeOptions) -> Result<(NodeOptions, NodeConfig)> {
    let config = read(&options.config_file)?;
    if options.controller.is_empty() {
        options.controller = config.controller.clone();
    }
    if options.name.is_empty() {
        options.name = config.name.clone();
    }
    validate_identity(&options.controller, &options.name)?;
    validate_transport(&options.controller, config.allow_insecure_http)?;
    Ok((options, config))
}

fn read(path: &Path) -> Result<NodeConfig> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let config: NodeConfig = toml::from_str(&raw).context("parse node configuration")?;
    ensure!(
        (1..=super::MAX_HEARTBEAT_SECONDS).contains(&config.heartbeat_seconds),
        "heartbeat_seconds must be between 1 and 10"
    );
    ensure!(
        (180..=super::MAX_LEASE_TIMEOUT_SECONDS).contains(&config.lease_timeout_seconds),
        "lease_timeout_seconds must be between 180 and 3600"
    );
    ensure!(
        !config.runtimes.is_empty(),
        "Configure at least one runtime, for example [runtimes.always-ready]"
    );
    for (name, commands) in &config.runtimes {
        ensure!(
            !name.is_empty() && name.len() <= 80,
            "Runtime names must contain 1 to 80 characters"
        );
        for command in [
            &commands.ensure,
            &commands.renew,
            &commands.release,
            &commands.stop,
            &commands.identity,
        ] {
            if let Some(program) = command.first() {
                ensure!(
                    Path::new(program).is_absolute(),
                    "Runtime {name}: command executable must be an absolute path"
                );
                let metadata = std::fs::metadata(program)
                    .with_context(|| format!("Runtime {name}: executable {program} is missing"))?;
                ensure!(
                    metadata.is_file(),
                    "Runtime {name}: executable must be a file"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    ensure!(
                        metadata.permissions().mode() & 0o111 != 0,
                        "Runtime {name}: command is not executable"
                    );
                }
            }
            ensure!(
                command.iter().all(|arg| !arg.contains('\0')),
                "Runtime arguments cannot contain NUL"
            );
        }
    }
    Ok(config)
}

pub(super) fn validate_identity(controller: &str, name: &str) -> Result<()> {
    let url = url::Url::parse(controller)
        .context("Set controller to the reachable poundlock URL in node.toml or --controller")?;
    ensure!(
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Controller must be an HTTP(S) URL without credentials, query or fragment"
    );
    ensure!(
        !name.trim().is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
        "Set name to a machine name of 1 to 80 characters"
    );
    Ok(())
}

/// How the node reaches its controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transport {
    Https,
    /// Plain HTTP that never leaves this machine.
    Loopback,
    /// Plain HTTP across a network: readable and forgeable by anyone on the path.
    PlainHttp,
}

pub(super) fn transport(controller: &str) -> Result<Transport> {
    let url = url::Url::parse(controller).context("parse the controller address")?;
    Ok(match url.scheme() {
        "https" => Transport::Https,
        "http" if is_loopback(&url) => Transport::Loopback,
        "http" => Transport::PlainHttp,
        other => bail!("unsupported controller scheme {other}"),
    })
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Plain HTTP to another machine hands the node credential, and the node's
/// runtime actions, to anyone on the network path. It takes the operator's
/// explicit `allow_insecure_http = true`.
pub(super) fn validate_transport(controller: &str, allow_insecure_http: bool) -> Result<()> {
    ensure!(
        allow_insecure_http || transport(controller)? != Transport::PlainHttp,
        "The controller address {controller} uses plain HTTP to another machine, so this node's \
         credential and the controller's instructions would cross the network unencrypted. Use an \
         https:// address, or set allow_insecure_http = true in node.toml to accept that risk on a \
         network you trust."
    );
    Ok(())
}

pub fn check_config(path: &Path) -> Result<Value> {
    check_config_identity(path, false)
}

pub fn check_config_identity(path: &Path, required: bool) -> Result<Value> {
    let config = read(path)?;
    if required || !config.controller.is_empty() || !config.name.is_empty() {
        validate_identity(&config.controller, &config.name)?;
        validate_transport(&config.controller, config.allow_insecure_http)?;
    }
    Ok(
        json!({"configuration":"valid", "runtimes":config.runtimes.keys().collect::<Vec<_>>(), "auto_update":config.auto_update,
            "update_key":super::update::embedded_key_text()}),
    )
}

pub async fn enroll_once(options: NodeOptions) -> Result<Value> {
    let (mut options, config) = load(options)?;
    config.network.validate()?;
    config.ollama.validate()?;
    ensure!(
        !config.runtimes.contains_key("ollama"),
        "The ollama runtime name is reserved for built-in model management"
    );
    let api = NodeClient::new(options.controller.clone(), &config.network)?;
    if !options.credential_file.exists() {
        let token = tokio::task::spawn_blocking(|| -> Result<String> {
            let mut token = String::new();
            // At a terminal, ask without echo, so the token is in neither the
            // command line nor shell history (L11). Piped input is read as is.
            let _quiet = Quiet::stdin();
            if _quiet.is_some() {
                eprint!("Enrollment token (input hidden): ");
                std::io::BufRead::read_line(
                    &mut std::io::BufReader::new(std::io::stdin().take(4097)),
                    &mut token,
                )?;
                eprintln!();
            } else {
                std::io::stdin().take(4097).read_to_string(&mut token)?;
            }
            ensure!(
                token.len() <= 4096 && !token.trim().is_empty(),
                "Provide a nonempty enrollment token on standard input"
            );
            Ok(token.trim().to_owned())
        })
        .await??;
        options.enrollment_token = Some(token);
    }
    let capabilities = super::capabilities(&config, &options.controller, &options.data_dir).await;
    let credential = super::load_or_enroll(&api, &options, &capabilities).await?;
    // Do not heartbeat: one-shot enrollment must not advertise a running service.
    let status = api.status(&credential).await?;
    ensure!(
        status["name"].as_str() == Some(options.name.as_str()),
        "Configured name differs from the enrolled machine; restore its name or enroll again"
    );
    Ok(
        json!({"enrolled":true,"node_id":credential.node_id,"status":status["status"],"capabilities":capabilities}),
    )
}

fn credential(path: &Path) -> Result<NodeCredential> {
    serde_json::from_slice(
        &std::fs::read(path).context("Node credential missing; run the installer to enroll")?,
    )
    .context("Read node credential")
}

pub async fn doctor(options: NodeOptions) -> Result<Value> {
    let (options, config) = load(options)?;
    config.network.validate()?;
    let api = NodeClient::new(options.controller, &config.network)?;
    let status = api.status(&credential(&options.credential_file)?).await?;
    let mut inhibitor = super::inhibitor::acquire("diagnostic").await?;
    if let Some(ref mut held) = inhibitor {
        held.stop().await;
    }
    if inhibitor.is_none() {
        bail!("Sleep inhibition is not supported on this operating system");
    }
    Ok(
        json!({"configuration":"valid","controller":"authenticated","node":status,"network":config.network.report(),"inventory":super::inventory::collect(&options.data_dir, &super::identity_commands(&config)).await,"sleep_inhibitor":"available"}),
    )
}

pub async fn revoke(options: NodeOptions) -> Result<Value> {
    let (options, config) = load(options)?;
    NodeClient::new(options.controller, &config.network)?
        .revoke(&credential(&options.credential_file)?)
        .await
}

pub async fn maintenance(options: NodeOptions, paused: bool) -> Result<Value> {
    let (options, config) = load(options)?;
    NodeClient::new(options.controller, &config.network)?
        .maintenance(&credential(&options.credential_file)?, paused)
        .await
}

/// Turns terminal echo off on standard input until dropped; `None` when
/// standard input is not a terminal.
struct Quiet(libc::termios);

impl Quiet {
    fn stdin() -> Option<Self> {
        // SAFETY: plain termios calls on file descriptor 0 with owned structs.
        unsafe {
            if libc::isatty(0) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            if libc::tcsetattr(0, libc::TCSANOW, &quiet) != 0 {
                return None;
            }
            Some(Self(saved))
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `stdin`.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_node_configuration_fails_before_enrollment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        for bad in [
            "heartbeat_seconds = 0\n[runtimes.ready]",
            "lease_timeout_seconds = 30\n[runtimes.ready]",
            "lease_timeout_seconds = 179\n[runtimes.ready]",
            "heartbeat_second = 3\n[runtimes.ready]",
            "[runtimes.ready]\nensure=['relative-command']",
            "[runtimes.ready]\nensur=[]",
            "[runtimes.ready]\nidentity=['relative-identity']",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(check_config(&path).is_err(), "{bad}");
        }
        std::fs::write(&path, super::super::example_config()).unwrap();
        assert!(check_config(&path).is_ok());
        // Self-update stays on unless the configuration turns it off; what it
        // may install is decided by the update checks, not by this setting.
        std::fs::write(&path, "[runtimes.ready]").unwrap();
        assert_eq!(check_config(&path).unwrap()["auto_update"], true);
        std::fs::write(&path, "auto_update = false\n[runtimes.ready]").unwrap();
        assert_eq!(check_config(&path).unwrap()["auto_update"], false);
        assert!(validate_identity("https://user:secret@example.test", "node").is_err());
        assert!(validate_identity("https://controller.example.test", "node").is_ok());
    }

    #[test]
    fn plain_http_to_another_machine_needs_the_explicit_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let config = |controller: &str, opt_in: &str| {
            format!("controller = '{controller}'\nname = 'node'\n{opt_in}[runtimes.ready]\n")
        };
        for remote in ["http://10.20.0.5:7331", "http://controller.lan:7331"] {
            std::fs::write(&path, config(remote, "")).unwrap();
            let error = check_config(&path).unwrap_err();
            assert!(
                error.to_string().contains("allow_insecure_http"),
                "{error:#}"
            );
            let options = NodeOptions {
                controller: String::new(),
                name: String::new(),
                enrollment_token: None,
                credential_file: dir.path().join("credential.json"),
                config_file: path.clone(),
                data_dir: dir.path().to_path_buf(),
            };
            assert!(load(options).is_err(), "{remote}");
            std::fs::write(&path, config(remote, "allow_insecure_http = true\n")).unwrap();
            assert!(check_config(&path).is_ok(), "{remote}");
        }
        // An address on the command line is held to the same rule.
        std::fs::write(&path, config("https://controller.lan", "")).unwrap();
        let options = NodeOptions {
            controller: "http://10.20.0.5:7331".into(),
            name: String::new(),
            enrollment_token: None,
            credential_file: dir.path().join("credential.json"),
            config_file: path.clone(),
            data_dir: dir.path().to_path_buf(),
        };
        assert!(load(options).is_err());
        for fine in [
            "https://controller.lan",
            "http://127.0.0.1:7331",
            "http://localhost:7331",
            "http://[::1]:7331",
        ] {
            std::fs::write(&path, config(fine, "")).unwrap();
            assert!(check_config(&path).is_ok(), "{fine}");
        }
    }

    #[test]
    fn identity_command_is_optional_and_validated_like_other_runtime_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");

        // Absent identity: still a valid runtime.
        std::fs::write(&path, "[runtimes.ready]\n").unwrap();
        let checked = check_config(&path).unwrap();
        assert_eq!(checked["runtimes"], json!(["ready"]));

        // A relative executable is rejected exactly like ensure/renew/etc.
        std::fs::write(&path, "[runtimes.ready]\nidentity=['relative-identity']\n").unwrap();
        assert!(check_config(&path).is_err());

        // A real absolute executable is accepted.
        std::fs::write(
            &path,
            "[runtimes.ready]\nidentity=['/bin/echo', 'identity']\n",
        )
        .unwrap();
        assert!(check_config(&path).is_ok());
    }
}
