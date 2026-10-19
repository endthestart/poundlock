use crate::domain::RunnerPackage;
use anyhow::{Result, bail};
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// Where Pi runs, which decides how far it may trust the project it works in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiMode {
    /// In a job's worker container, which holds nothing of the host's. The
    /// project's own Pi settings and extensions may load there.
    Container,
    /// On the host as the user, in an editor's working tree (attached ACP).
    /// Nothing from the project may run code, and Pi sees none of the user's
    /// environment.
    Attached,
}

/// The only host variables attached Pi inherits: what it needs to find its
/// tools, format text, use a temporary folder and trust a private CA in front
/// of the controller. Everything else, including `POUNDLOCK_TOKEN`, provider
/// keys, `SSH_AUTH_SOCK` and `NODE_OPTIONS`, stays behind.
const ATTACHED_ENVIRONMENT: &[&str] = &["PATH", "LANG", "TERM", "TMPDIR", "NODE_EXTRA_CA_CERTS"];

fn attached_environment(
    host: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    host.into_iter()
        .filter(|(name, _)| ATTACHED_ENVIRONMENT.iter().any(|kept| name == kept))
        .collect()
}

pub struct PiLaunch<'a> {
    pub mode: PiMode,
    pub prompt: &'a str,
    pub model_id: &'a str,
    pub context_tokens: u64,
    pub max_output_tokens: u64,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub extra_note: &'a str,
    pub instruction_files: &'a [String],
}

pub async fn pi_command(
    package: &RunnerPackage,
    attempt_dir: &Path,
    workspace: &crate::git::Workspace,
) -> Result<Command> {
    if package.harness.kind != "pi" {
        bail!("unsupported harness kind: {}", package.harness.kind);
    }
    let mut extra_note =
        String::from("\nRepository instructions (each applies to its named root):");
    for root in &workspace.worktrees {
        extra_note.push_str(&format!("\nRoot: {}", root.root_name));
        extra_note
            .push_str(&instruction_preamble(&root.path, &package.project.instruction_files).await?);
    }
    pi_launch(
        &PiLaunch {
            mode: PiMode::Container,
            prompt: &package.job.prompt,
            model_id: &package.model.model_id,
            context_tokens: package.model.context_tokens,
            max_output_tokens: package.model.max_output_tokens,
            base_url: &package.endpoint.endpoint.base_url,
            api_key: &package.endpoint.api_key,
            extra_note: &extra_note,
            instruction_files: &[],
        },
        attempt_dir,
        &workspace.cwd,
    )
    .await
}

// Pi 1.1.0 keeps 4096 context tokens free on every request (pi-ai's
// `CONTEXT_SAFETY_TOKENS`) before accounting for the task and tools.
pub fn validate_pi_context(context_tokens: u64) -> Result<()> {
    anyhow::ensure!(
        context_tokens >= 8192,
        "Pi coding tasks require a context limit of at least 8192 tokens. Configure that limit in the model server and in the Poundlock model profile before submitting."
    );
    Ok(())
}

pub async fn pi_launch(launch: &PiLaunch<'_>, attempt_dir: &Path, cwd: &Path) -> Result<Command> {
    validate_pi_context(launch.context_tokens)?;
    let home = attempt_dir.join("pi-home");
    let agent_dir = home.join(".pi/agent");
    tokio::fs::create_dir_all(&agent_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).await?;
    }
    // Pi never sends a reasoning parameter: the profile's thinking default is
    // applied by Poundlock on both the relay and the job-inference path.
    let model = json!({"id":launch.model_id,"name":launch.model_id,"reasoning":false,
        "input":["text"],"contextWindow":launch.context_tokens,
        "maxTokens":launch.max_output_tokens,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}});
    // No strict tool schemas. Pi 0.86 makes every optional argument required
    // under strict sampling, and local models then fill them in: a 30-second
    // bash timeout or a 100-line read limit on every call.
    let models = json!({"providers":{"poundlock":{"baseUrl":launch.base_url,
        "api":"openai-completions","authHeader":!launch.api_key.is_empty(),
        "compat":{"supportsDeveloperRole":false,"supportsReasoningEffort":false,
            "supportsUsageInStreaming":true,"maxTokensField":"max_tokens",
            "supportsStrictMode":false},
        "models":[model]}}});
    // The pinned Pi resolver evaluates credential strings in both configuration
    // and auth storage. Escape its literal syntax before storing a private key.
    // The pinned Pi CLI requires a nonempty credential even for keyless local
    // servers. Its documented placeholder is not an access credential.
    let key = if launch.api_key.is_empty() {
        "poundlock-local-no-auth"
    } else {
        launch.api_key
    };
    let literal_key = key.replace('$', "$$").replace('!', "$!");
    let auth = json!({"poundlock":{"type":"api_key","key":literal_key}});
    crate::crypto::write_private(&agent_dir.join("auth.json"), &auth.to_string())?;
    crate::crypto::write_private(
        &agent_dir.join("models.json"),
        &serde_json::to_string_pretty(&models)?,
    )?;
    // Project trust is what lets a repository's `.pi` settings and TypeScript
    // extensions load. Harmless in a job container; on the host it would run
    // a repository's code as the user before the model says a word.
    let (trust, project) = match launch.mode {
        PiMode::Container => ("always", &["--approve"][..]),
        PiMode::Attached => (
            "never",
            &[
                "--no-approve",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-context-files",
            ][..],
        ),
    };
    crate::crypto::write_private(
        &agent_dir.join("settings.json"),
        &json!({"defaultProjectTrust":trust,"enableInstallTelemetry":false}).to_string(),
    )?;
    let instructions = instruction_preamble(cwd, launch.instruction_files).await?;
    let mut command = Command::new("pi");
    if launch.mode == PiMode::Attached {
        command
            .env_clear()
            .envs(attached_environment(std::env::vars_os()));
    }
    command
        .current_dir(cwd)
        // Pi 1.0.4+ keeps MCP tools through `--tools`; no job uses MCP.
        .args(["--mode", "json", "--no-session", "--no-mcp"])
        .args(project)
        .args(["--provider", "poundlock", "--model"])
        .arg(launch.model_id)
        .args(["--thinking", "off", "--tools", "read,write,edit,bash"])
        .arg(format!(
            "{}{}{instructions}",
            launch.prompt, launch.extra_note
        ))
        .env("HOME", &home)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_TELEMETRY", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    Ok(command)
}

pub async fn instruction_preamble(cwd: &Path, configured: &[String]) -> Result<String> {
    let mut files = configured.to_vec();
    if !files.iter().any(|file| file == "AGENTS.md") && cwd.join("AGENTS.md").is_file() {
        files.insert(0, "AGENTS.md".into());
    }
    if files.is_empty() {
        return Ok(String::new());
    }
    let allowed = tokio::fs::canonicalize(cwd).await?;
    let mut blocks = vec!["Required repository instructions:".to_owned()];
    for relative in &files {
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("instruction file must be a relative path within its repository: {relative}");
        }
        let path = tokio::fs::canonicalize(cwd.join(relative))
            .await
            .map_err(|_| anyhow::anyhow!("required instruction file is unavailable: {relative}"))?;
        if !path.starts_with(&allowed) {
            bail!("instruction file escapes its repository: {relative}");
        }
        let body = tokio::fs::read_to_string(path).await?;
        // Do not silently discard a policy's final requirements.
        if body.len() > 32 * 1024 {
            bail!("instruction file exceeds 32 KiB: {relative}");
        }
        blocks.push(format!("----- {relative} -----\n{body}"));
    }
    Ok(format!("\n\n{}", blocks.join("\n")))
}

pub fn bounded_output(mut stdout: Vec<u8>, mut stderr: Vec<u8>, limit: usize) -> String {
    stdout.extend_from_slice(b"\n--- stderr ---\n");
    stdout.append(&mut stderr);
    if stdout.len() > limit {
        let half = limit / 2;
        let mut value = stdout[..half].to_vec();
        value.extend_from_slice(b"\n... output truncated ...\n");
        value.extend_from_slice(&stdout[stdout.len() - half..]);
        stdout = value;
    }
    String::from_utf8_lossy(&stdout).into_owned()
}

// Pi can exit zero after a failed model request. Use the final assistant event,
// allowing a later successful retry to supersede an earlier failure.
pub fn pi_failure(output: &str) -> Option<serde_json::Value> {
    let mut failure = None;
    for line in output
        .split("\n--- stderr ---\n")
        .next()
        .unwrap_or_default()
        .lines()
    {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event["type"] == "message_end" && event["message"]["role"] == "assistant" {
            failure = match event["message"]["stopReason"].as_str() {
                Some("error" | "aborted") => {
                    Some(json!({"layer":"harness","code":"model_request_failed",
                    "message":"The model request failed. Inspect the retained agent output.","retry_safe":false}))
                }
                Some("length") => Some(json!({"layer":"harness","code":"model_output_limit",
                    "message":"The model reached its output limit before finishing. Review the retained patch and adjust the model profile output or context limit before retrying.","retry_safe":false})),
                _ => None,
            };
        }
    }
    failure
}

// A truncated capture only means the runner discarded bytes past its output
// budget (see `Docker::exec`); Pi's own process still runs to completion
// either way. When the agent's own final turn already reached a normal
// terminal state before that happened, the retained patch reflects finished
// work and is worth trusting even though the attempt still reports an
// output-limit failure. Mirrors `pi_failure`'s scan so both agree on what
// counts as a terminal, non-error `message_end`.
pub fn agent_turn_settled(output: &str) -> bool {
    let mut settled = false;
    for line in output
        .split("\n--- stderr ---\n")
        .next()
        .unwrap_or_default()
        .lines()
    {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event["type"] == "message_end" && event["message"]["role"] == "assistant" {
            // Only a terminal stop reason. `toolUse` ends the *message* before
            // the tool runs, and the next `turn_start` arrives only once the
            // tool has returned — so a denylist reads the entire
            // tool-execution window, where a cutoff most often lands, as
            // finished work. An unfamiliar reason is not evidence either.
            settled = event["message"]["stopReason"].as_str() == Some("stop");
        }
        // A turn that has begun and not yet ended is work in flight, whatever
        // the previous turn concluded: the agent had more to do and was
        // stopped partway through it.
        if event["type"] == "turn_start" {
            settled = false;
        }
    }
    settled
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn launched_models_json() -> serde_json::Value {
        let attempt_dir = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        pi_launch(
            &PiLaunch {
                mode: PiMode::Container,
                prompt: "do work",
                model_id: "local-model",
                context_tokens: 8192,
                max_output_tokens: 1024,
                base_url: "http://127.0.0.1:11434/v1",
                api_key: "",
                extra_note: "",
                instruction_files: &[],
            },
            attempt_dir.path(),
            cwd.path(),
        )
        .await
        .unwrap();
        let raw =
            tokio::fs::read_to_string(attempt_dir.path().join("pi-home/.pi/agent/models.json"))
                .await
                .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    async fn launched(mode: PiMode) -> (tempfile::TempDir, Command, serde_json::Value) {
        let attempt_dir = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let command = pi_launch(
            &PiLaunch {
                mode,
                prompt: "do work",
                model_id: "local-model",
                context_tokens: 8192,
                max_output_tokens: 1024,
                base_url: "http://127.0.0.1:7331/v1",
                api_key: "application-token",
                extra_note: "",
                instruction_files: &[],
            },
            attempt_dir.path(),
            cwd.path(),
        )
        .await
        .unwrap();
        let settings =
            std::fs::read_to_string(attempt_dir.path().join("pi-home/.pi/agent/settings.json"))
                .unwrap();
        (
            attempt_dir,
            command,
            serde_json::from_str(&settings).unwrap(),
        )
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn pi_never_loads_mcp_in_either_mode() {
        for mode in [PiMode::Container, PiMode::Attached] {
            let (_keep, command, _settings) = launched(mode).await;
            assert!(args(&command).iter().any(|arg| arg == "--no-mcp"));
        }
    }

    #[tokio::test]
    async fn attached_pi_trusts_nothing_from_the_project_and_inherits_no_secrets() {
        let (_keep, command, settings) = launched(PiMode::Attached).await;
        assert_eq!(settings["defaultProjectTrust"], "never");
        let args = args(&command);
        for flag in [
            "--no-approve",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
        ] {
            assert!(
                args.iter().any(|arg| arg == flag),
                "{flag} missing: {args:?}"
            );
        }
        assert!(!args.iter().any(|arg| arg == "--approve"), "{args:?}");
        // The host environment is cleared; only the allowlist and Pi's own
        // settings are set on top of it.
        assert!(format!("{:?}", command.as_std()).contains("env -i "));
        for (name, _) in command.as_std().get_envs() {
            let name = name.to_string_lossy();
            assert!(
                ATTACHED_ENVIRONMENT.contains(&name.as_ref())
                    || [
                        "HOME",
                        "PI_CODING_AGENT_DIR",
                        "PI_OFFLINE",
                        "PI_SKIP_VERSION_CHECK",
                        "PI_TELEMETRY"
                    ]
                    .contains(&name.as_ref()),
                "{name} reached attached Pi"
            );
        }
    }

    #[test]
    fn attached_pi_keeps_only_the_allowlisted_host_variables() {
        let host = [
            ("PATH", "/usr/bin:/bin"),
            ("LANG", "en_US.UTF-8"),
            ("TERM", "xterm-256color"),
            ("TMPDIR", "/tmp/"),
            ("POUNDLOCK_TOKEN", "application-token"),
            ("OPENAI_API_KEY", "provider-key"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("NODE_OPTIONS", "--require /tmp/evil.js"),
            ("HOME", "/Users/owner"),
            ("PI_CODING_AGENT_DIR", "/Users/owner/.pi/agent"),
        ]
        .map(|(name, value)| (name.into(), value.into()));
        let kept: Vec<String> = attached_environment(host)
            .into_iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept, ["PATH", "LANG", "TERM", "TMPDIR"]);
    }

    #[tokio::test]
    async fn container_pi_keeps_project_trust_inside_its_sandbox() {
        let (_keep, command, settings) = launched(PiMode::Container).await;
        assert_eq!(settings["defaultProjectTrust"], "always");
        assert!(args(&command).iter().any(|arg| arg == "--approve"));
        assert!(!format!("{:?}", command.as_std()).contains("env -i "));
    }

    #[tokio::test]
    async fn pi_never_sends_a_reasoning_parameter_because_the_server_owns_it() {
        let models = launched_models_json().await;
        let provider = &models["providers"]["poundlock"];
        assert_eq!(provider["compat"]["supportsReasoningEffort"], false);
        assert_eq!(provider["models"][0]["reasoning"], false);
        assert!(provider["models"][0].get("thinkingLevelMap").is_none());
    }

    #[tokio::test]
    async fn pi_sends_tool_schemas_without_strict_sampling() {
        let models = launched_models_json().await;
        assert_eq!(
            models["providers"]["poundlock"]["compat"]["supportsStrictMode"],
            false
        );
    }

    #[test]
    fn model_error_is_not_a_successful_no_change_and_later_recovery_wins() {
        let failed = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Connection error."}}"#;
        assert_eq!(pi_failure(failed).unwrap()["code"], "model_request_failed");
        let recovered = format!(
            "{failed}\n{}",
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}"#
        );
        assert!(pi_failure(&recovered).is_none());
    }

    #[test]
    fn agent_turn_settled_requires_a_terminal_non_error_message_end() {
        assert!(!agent_turn_settled(""));
        assert!(!agent_turn_settled("not json\nneither is this"));
        let settled =
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}"#;
        assert!(agent_turn_settled(settled));
        let mid_turn_cutoff = r#"{"type":"message_start","message":{"role":"assistant"}}
{"type":"tool_call","name":"bash"#; // truncated mid-line, no terminal event
        assert!(!agent_turn_settled(mid_turn_cutoff));
        let failed =
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error"}}"#;
        assert!(!agent_turn_settled(failed));
        let length_limited =
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"length"}}"#;
        assert!(!agent_turn_settled(length_limited));
        // A later turn's own settlement supersedes an earlier one, matching
        // `pi_failure`'s "later recovery wins" semantics.
        let recovered = format!("{failed}\n{settled}");
        assert!(agent_turn_settled(&recovered));
        // Stray content after the marker (e.g. an appended stderr section)
        // does not retroactively unsettle a completed turn.
        let with_stderr = format!("{settled}\n--- stderr ---\nsome warning\n");
        assert!(agent_turn_settled(&with_stderr));
        // A turn that started after the last settled message is work in
        // flight: the agent was cut off partway through it, not after it.
        let next_turn_begun =
            format!("{settled}\n{{\"type\":\"turn_end\"}}\n{{\"type\":\"turn_start\"}}");
        assert!(!agent_turn_settled(&next_turn_begun));
    }

    // Pi ends the assistant message *before* running the tool it asked for,
    // and the next `turn_start` only arrives once the tool has returned. So
    // the whole tool-execution window — the window a wall cutoff is most
    // likely to land in — sits after a `message_end` and before any
    // `turn_start`. Only a terminal stop reason means the agent was done.
    #[test]
    fn a_cutoff_while_a_tool_is_running_is_not_a_settled_turn() {
        let cut_off_running_a_tool = r#"{"type":"turn_start"}
{"type":"message_end","message":{"role":"assistant","stopReason":"toolUse"}}
{"type":"tool_execution_start","name":"bash"}"#;
        assert!(!agent_turn_settled(cut_off_running_a_tool));

        // Even once the tool returns, the agent has not spoken again.
        let tool_returned = format!(
            "{cut_off_running_a_tool}\n{}",
            r#"{"type":"tool_execution_end","name":"bash"}"#
        );
        assert!(!agent_turn_settled(&tool_returned));

        // An unfamiliar stop reason is not evidence of finished work either.
        let unknown_reason =
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"refusal"}}"#;
        assert!(!agent_turn_settled(unknown_reason));
        let absent_reason = r#"{"type":"message_end","message":{"role":"assistant"}}"#;
        assert!(!agent_turn_settled(absent_reason));

        // The full shape of a finished turn still settles.
        let finished = format!(
            "{cut_off_running_a_tool}\n{}\n{}\n{}",
            r#"{"type":"tool_execution_end","name":"bash"}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}"#,
            r#"{"type":"turn_end"}"#
        );
        assert!(agent_turn_settled(&finished));
    }

    // Pi 0.86 opens the stream with the transcript's system message. It is not
    // an assistant message, so it neither fails nor settles an attempt, and a
    // request Pi retried after a failure still reads as recovered.
    #[test]
    fn the_leading_system_message_does_not_change_the_outcome() {
        let system = r#"{"type":"message_end","message":{"role":"system","content":"","sections":{"preamble":"You are an expert coding assistant"}}}"#;
        let failed = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"503"}}"#;
        let retrying = r#"{"type":"agent_end","willRetry":true}"#;
        let retried = format!(
            "{system}\n{failed}\n{retrying}\n{}\n{}\n{}\n{}",
            r#"{"type":"turn_start"}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}"#,
            r#"{"type":"agent_end","willRetry":false}"#,
            r#"{"type":"agent_settled"}"#
        );
        assert!(pi_failure(&retried).is_none());
        assert!(agent_turn_settled(&retried));

        let gave_up = format!(
            "{system}\n{failed}\n{}\n{}",
            r#"{"type":"agent_end","willRetry":false}"#, r#"{"type":"agent_settled"}"#
        );
        assert_eq!(
            pi_failure(&gave_up).unwrap()["code"],
            "model_request_failed"
        );
        assert!(!agent_turn_settled(&gave_up));
        assert!(pi_failure(system).is_none());
        assert!(!agent_turn_settled(system));
    }

    #[test]
    fn unusable_pi_context_and_exhausted_output_are_not_successes() {
        assert!(validate_pi_context(4096).is_err());
        assert!(validate_pi_context(8192).is_ok());
        let output =
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"length"}}"#;
        assert_eq!(pi_failure(output).unwrap()["code"], "model_output_limit");
    }

    #[tokio::test]
    async fn preamble_includes_agents_and_configured_files() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("AGENTS.md"),
            "follow the tutorial contract",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join("docs/tutorial")).unwrap();
        std::fs::write(
            directory.path().join("docs/tutorial/README.md"),
            "approve before writing",
        )
        .unwrap();
        let text = instruction_preamble(directory.path(), &["docs/tutorial/README.md".into()])
            .await
            .unwrap();
        assert!(text.contains("AGENTS.md"));
        assert!(text.contains("follow the tutorial contract"));
        assert!(text.contains("approve before writing"));
    }

    #[tokio::test]
    async fn required_instructions_fail_closed_on_missing_escape_and_oversize() {
        let directory = tempdir().unwrap();
        assert!(
            instruction_preamble(directory.path(), &["missing.md".into()])
                .await
                .is_err()
        );
        assert!(
            instruction_preamble(directory.path(), &["../outside.md".into()])
                .await
                .is_err()
        );
        std::fs::write(directory.path().join("AGENTS.md"), "€".repeat(12000)).unwrap();
        assert!(instruction_preamble(directory.path(), &[]).await.is_err());
        #[cfg(unix)]
        {
            let outside = tempdir().unwrap();
            std::fs::write(outside.path().join("secret"), "secret").unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("secret"),
                directory.path().join("escape.md"),
            )
            .unwrap();
            assert!(
                instruction_preamble(directory.path(), &["escape.md".into()])
                    .await
                    .is_err()
            );
        }
    }
}
