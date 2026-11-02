mod args;

use anyhow::{Result, bail};
use args::{Args, Command};
use clap::Parser;
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("poundlock=info".parse()?),
        )
        .init();
    match Args::parse().command {
        Command::InspectCandidate => print(poundlock::runner::inspect_candidate().await?),
        Command::FetchSource {
            repository,
            revision,
            into,
            copy_to,
        } => {
            let credentials = match (
                std::env::var("POUNDLOCK_GIT_USERNAME"),
                std::env::var("POUNDLOCK_GIT_TOKEN"),
            ) {
                (Ok(username), Ok(token)) => {
                    Some(poundlock::git_source::GitCredentials { username, token })
                }
                _ => None,
            };
            // One line on stderr: the supervisor reports its last line.
            match poundlock::git_source::fetch_source(
                &into,
                &copy_to,
                &repository,
                &revision,
                credentials.as_ref(),
            )
            .await
            {
                Ok(base) => print(serde_json::json!({ "base_revision": base })),
                Err(error) => {
                    eprintln!("{error:#}");
                    std::process::exit(1);
                }
            }
        }
        Command::RunCheck { cwd, command } => {
            poundlock::runner::verification_command(&cwd, &command)?
        }
        Command::WakeRelay { socket } => poundlock::wake::run_relay(&socket).await?,
        Command::NodeInterfaces => print(serde_json::to_value(
            poundlock::node::network::interfaces()?,
        )?),
        Command::Backup { to, data_dir } => {
            print(serde_json::to_value(poundlock::backup::back_up(
                &data_dir.join("poundlock.sqlite3"),
                &to,
                chrono::Utc::now(),
            )?)?)
        }
        Command::Controller => {
            poundlock::server::run(poundlock::config::AppConfig::load_controller()?).await?
        }
        Command::Runner {
            controller,
            token_file,
            projects_dir,
            work_dir,
            result_dir,
            poll_seconds,
            wall_seconds,
            capacity,
            docker_socket,
            worker_runtime,
        } => {
            poundlock::runner::run(poundlock::runner::RunnerOptions {
                controller,
                token_file,
                projects_dir,
                work_dir,
                result_dir,
                poll_seconds,
                wall_seconds,
                capacity,
                docker_socket,
                worker_runtime: (!worker_runtime.is_empty()).then_some(worker_runtime),
            })
            .await?
        }
        Command::Node {
            controller,
            name,
            credential_file,
            config,
            data_dir,
        } => {
            poundlock::node::run(poundlock::node::NodeOptions {
                controller,
                name,
                // A container has no terminal for node-enroll, so its first
                // start may take the token from the environment instead.
                enrollment_token: std::env::var("POUNDLOCK_ENROLLMENT_TOKEN")
                    .ok()
                    .filter(|token| !token.is_empty()),
                credential_file,
                config_file: config,
                data_dir: data_dir.unwrap_or_else(poundlock::node::default_data_dir),
            })
            .await?
        }
        Command::Acp {
            controller,
            token,
            token_file,
            execution,
            project_id,
            model_profile_id,
            harness_profile_id,
            workspace_root,
            instruction_file,
        } => {
            poundlock::acp::run(poundlock::acp::AcpOptions {
                controller,
                token: resolve_token(token, token_file.as_deref())?,
                execution,
                project_id,
                model_profile_id,
                harness_profile_id,
                workspace_root,
                instruction_files: instruction_file,
            })
            .await?
        }
        Command::Remote(command) => poundlock::cli::run(command, &mut std::io::stdout()).await?,
        Command::NodeCheck {
            config,
            require_identity,
        } => print(poundlock::node::check_config_identity(
            &config,
            require_identity,
        )?),
        Command::NodeEnroll(files) => print(poundlock::node::enroll_once(files.options()).await?),
        Command::NodeDoctor(files) => print(poundlock::node::doctor(files.options()).await?),
        Command::NodePause(files) => {
            print(poundlock::node::maintenance(files.options(), true).await?)
        }
        Command::NodeResume(files) => {
            print(poundlock::node::maintenance(files.options(), false).await?)
        }
        Command::NodeRevoke(files) => print(poundlock::node::revoke(files.options()).await?),
    }
    Ok(())
}

fn print(value: Value) {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}

fn resolve_token(token: Option<String>, token_file: Option<&std::path::Path>) -> Result<String> {
    match (token, token_file) {
        (Some(token), _) => Ok(token),
        (None, Some(path)) => poundlock::config::load_token(path),
        (None, None) => bail!("--token or --token-file is required"),
    }
}
