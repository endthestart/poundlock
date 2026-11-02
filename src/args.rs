use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version = poundlock::VERSION,
    about = "Self-hosted control plane for coding agents and private inference"
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    Controller,
    /// Write one checked, compressed copy of the controller's database to a
    /// folder, as the nightly backup does (the master key is not copied).
    Backup {
        /// The folder for the copy; it also gets last-backup.json.
        #[arg(long)]
        to: std::path::PathBuf,
        /// The controller's data folder.
        #[arg(long, env = "POUNDLOCK_DATA_DIR", default_value = "./data")]
        data_dir: std::path::PathBuf,
    },
    #[command(hide = true)]
    InspectCandidate,
    /// Run in a job's fetch container: check out a Git source (L13). The
    /// credential, if any, comes from POUNDLOCK_GIT_USERNAME and _TOKEN.
    #[command(hide = true)]
    FetchSource {
        #[arg(long)]
        repository: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        into: PathBuf,
        #[arg(long)]
        copy_to: PathBuf,
    },
    #[command(hide = true)]
    RunCheck {
        cwd: PathBuf,
        command: String,
    },
    /// Send wake packets from a Linux host network through a private Unix socket.
    WakeRelay {
        #[arg(long, default_value = "/run/poundlock-wake/wake.sock")]
        socket: PathBuf,
    },
    /// List local network interfaces, addresses and MACs before node enrollment.
    NodeInterfaces,
    Runner {
        #[arg(
            long,
            env = "POUNDLOCK_CONTROLLER_URL",
            default_value = "http://controller:7331"
        )]
        controller: String,
        #[arg(
            long,
            env = "POUNDLOCK_RUNNER_TOKEN_FILE",
            default_value = "/run/poundlock/runner-token"
        )]
        token_file: PathBuf,
        #[arg(long, env = "POUNDLOCK_PROJECTS_DIR", default_value = "/projects")]
        projects_dir: PathBuf,
        #[arg(long, env = "POUNDLOCK_WORK_DIR", default_value = "/work")]
        work_dir: PathBuf,
        #[arg(long, env = "POUNDLOCK_RESULT_DIR", default_value = "/results")]
        result_dir: PathBuf,
        #[arg(long, default_value_t = 2)]
        poll_seconds: u64,
        #[arg(long, env = "POUNDLOCK_JOB_WALL_SECONDS", default_value_t = 3600)]
        wall_seconds: u64,
        /// Maximum simultaneous jobs; inference capacity is reserved separately.
        #[arg(long, env = "POUNDLOCK_RUNNER_CAPACITY", default_value_t = 1,
            value_parser = clap::value_parser!(u16).range(1..=16))]
        capacity: u16,
        #[arg(
            long,
            env = "POUNDLOCK_DOCKER_SOCKET",
            default_value = "/var/run/docker.sock"
        )]
        docker_socket: PathBuf,
        /// A Docker runtime for job containers, such as `runsc` for gVisor (L16).
        /// Empty uses the engine's default.
        #[arg(long, env = "POUNDLOCK_WORKER_RUNTIME", default_value = "")]
        worker_runtime: String,
    },
    Node {
        #[arg(long, env = "POUNDLOCK_CONTROLLER_URL", default_value = "")]
        controller: String,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long, default_value = "./node-credential.json")]
        credential_file: PathBuf,
        #[arg(long, default_value = "./node.toml")]
        config: PathBuf,
        #[arg(long, env = "POUNDLOCK_NODE_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    Acp {
        #[arg(
            long,
            env = "POUNDLOCK_CONTROLLER_URL",
            default_value = "http://127.0.0.1:7331"
        )]
        controller: String,
        #[arg(long, env = "POUNDLOCK_TOKEN", hide_env_values = true)]
        token: Option<String>,
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long, default_value = "attached")]
        execution: String,
        #[arg(long)]
        project_id: Option<String>,
        #[arg(long)]
        model_profile_id: String,
        #[arg(long, default_value = "pi")]
        harness_profile_id: String,
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        #[arg(long)]
        instruction_file: Vec<String>,
    },
    // The admin CLI and the job commands: plain clients of a running
    // controller's HTTP API (see `poundlock::cli`).
    #[command(flatten)]
    Remote(poundlock::cli::RemoteCommand),
    /// Validate configuration without registering a node or running runtime commands.
    NodeCheck {
        #[arg(long, default_value = "./node.toml")]
        config: PathBuf,
        #[arg(long)]
        require_identity: bool,
    },
    /// Register once. Reads the enrollment token from standard input.
    NodeEnroll(NodeFiles),
    /// Check configuration, controller authentication and sleep inhibition.
    NodeDoctor(NodeFiles),
    /// Revoke this idle node credential before removing its files.
    NodeRevoke(NodeFiles),
    /// Stop accepting runtime actions while idle for safe maintenance.
    NodePause(NodeFiles),
    /// Accept runtime actions again after maintenance.
    NodeResume(NodeFiles),
}

#[derive(clap::Args)]
pub struct NodeFiles {
    #[arg(long, default_value = "/etc/poundlock/node.toml")]
    pub config: PathBuf,
    #[arg(long, default_value = "/var/lib/poundlock/node-credential.json")]
    pub credential_file: PathBuf,
}
impl NodeFiles {
    pub fn options(self) -> poundlock::node::NodeOptions {
        poundlock::node::NodeOptions {
            controller: String::new(),
            name: String::new(),
            enrollment_token: None,
            config_file: self.config,
            credential_file: self.credential_file,
            data_dir: poundlock::node::default_data_dir(),
        }
    }
}
