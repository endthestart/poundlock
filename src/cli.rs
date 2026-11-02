//! The `poundlock` subcommands that drive a running controller. Each is a plain
//! client of the controller's HTTP API through `ControllerClient`: no database,
//! no local state, nothing the API does not offer to every other client.
//!
//! Two surfaces, kept apart:
//! - **Administration** (`AdminCommand`): the operator API the console uses,
//!   with the administrator token.
//! - **Consumption** (`JobCommand`): the job API that Dispatch and other
//!   applications use, with the administrator token or an application token.

use crate::controller_client::{ControllerClient, resource_id};
use crate::domain::{CreateEndpoint, CreateProject, SubmitJob};
use anyhow::{Result, bail};
use clap::Subcommand;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Subcommand, Debug)]
pub enum RemoteCommand {
    #[command(flatten)]
    Admin(AdminCommand),
    #[command(flatten)]
    Jobs(JobCommand),
}

/// Where the controller is and how to authenticate to it.
#[derive(clap::Args, Clone, Debug)]
pub struct Remote {
    /// The controller's address.
    #[arg(
        long,
        env = "POUNDLOCK_CONTROLLER_URL",
        default_value = "http://127.0.0.1:7331"
    )]
    pub controller: String,
    /// The bearer token. Prefer --token-file: other users of this machine can
    /// see a token on the command line.
    #[arg(long, env = "POUNDLOCK_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// A file holding the token, such as /data/admin-token in the controller
    /// container. It wins over --token and POUNDLOCK_TOKEN.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
}

impl Remote {
    pub fn client(&self) -> Result<ControllerClient> {
        let token = match &self.token_file {
            Some(path) => crate::config::load_token(path)?,
            None => self.token.clone().unwrap_or_default(),
        };
        Ok(ControllerClient::new(&self.controller, &token)?)
    }
}

/// Administration: the operator API, administrator token.
#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Administration: print the controller overview (endpoints, models,
    /// projects, machines, tokens and recent jobs).
    Status(Remote),
    /// Administration: add a model server or gateway endpoint.
    EndpointAdd {
        #[command(flatten)]
        remote: Remote,
        #[arg(long)]
        name: String,
        /// `openai` for a model server, `litellm` for a gateway.
        #[arg(long, default_value = "openai")]
        kind: String,
        #[arg(long)]
        base_url: String,
        /// The endpoint's API key, if it needs one.
        #[arg(
            long,
            env = "POUNDLOCK_ENDPOINT_API_KEY",
            hide_env_values = true,
            default_value = ""
        )]
        api_key: String,
        /// The address operators open for this server, if it has one.
        #[arg(long)]
        public_url: Option<String>,
        /// Where a model server sends prompts: unknown, operator_network or
        /// external. Every model on it inherits it; a gateway declares it per
        /// route instead.
        #[arg(long)]
        data_destination: Option<String>,
    },
    /// Administration: add a model profile on an endpoint. What is left out,
    /// the controller fills in as the console's Add model does.
    ModelAdd(NewModel),
    /// Administration: register a server-side project (repository roots).
    ProjectAdd {
        #[command(flatten)]
        remote: Remote,
        #[arg(long)]
        name: String,
        #[arg(long, required = true)]
        root: Vec<String>,
        #[arg(long)]
        instruction_file: Vec<String>,
    },
    /// Administration: create a one-time machine enrollment token.
    NodeToken {
        #[command(flatten)]
        remote: Remote,
        /// How long the token is valid. The controller's default applies when
        /// omitted.
        #[arg(long)]
        valid_minutes: Option<i64>,
        /// The machine this token may enroll. Required to re-enroll one.
        #[arg(long)]
        node_name: Option<String>,
    },
    /// Administration: set a machine's Wake-on-LAN MAC and broadcast address.
    NodeWol {
        #[command(flatten)]
        remote: Remote,
        id: String,
        #[arg(long)]
        mac: String,
        #[arg(long, default_value = "255.255.255.255:9")]
        broadcast: String,
    },
    /// Administration: print the controller's example node.toml.
    NodeConfigExample(Remote),
}

/// A model profile, as `POST /api/v1/model-profiles` takes it. A value left
/// out is not sent: the controller fills it from its draft check, the rules
/// the console's Add model shows, so the two cannot disagree.
#[derive(clap::Args, Debug, Serialize)]
pub struct NewModel {
    #[command(flatten)]
    #[serde(skip)]
    pub remote: Remote,
    /// The endpoint the model is reached through.
    #[arg(long)]
    pub endpoint_id: String,
    /// What the server calls the model.
    #[arg(long)]
    pub model_id: String,
    /// What clients send. It cannot change later.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Required when nothing reports the model's context.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// The longest answer a request may ask for.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// What the profile may serve: durable, interactive or completion. Repeat
    /// the flag or separate with commas.
    #[arg(long, value_delimiter = ',')]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// never, manual or on-demand.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_policy: Option<String>,
    /// The machine that runs the model. Needs --runtime-profile.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// The runtime on that machine, as it reports it (for example llamacpp).
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_profile: Option<String>,
    /// model_default or off.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// A label for people; never used for routing.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_label: Option<String>,
    /// Another name clients may send. Repeat for more.
    #[arg(long = "alias")]
    pub aliases: Vec<String>,
    /// Ollama only: seconds to keep the model loaded after its last request.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_seconds: Option<u64>,
    /// A gateway route's own declaration of where it sends prompts: unknown,
    /// operator_network or external. A model server's declaration applies
    /// to every model on it.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_destination: Option<String>,
}

/// Consumption: the job API, administrator or application token.
#[derive(Subcommand, Debug)]
pub enum JobCommand {
    /// Consumption: submit a durable job on a registered project.
    JobSubmit {
        #[command(flatten)]
        remote: Remote,
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        model_profile_id: String,
        #[arg(long, default_value = "pi")]
        harness_profile_id: String,
        #[arg(long)]
        prompt: String,
        /// Resubmitting with the same key returns the same job.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Follow the job until it ends.
        #[arg(long)]
        watch: bool,
    },
    /// Consumption: print a job's events until it ends, then the job.
    JobWatch {
        #[command(flatten)]
        remote: Remote,
        id: String,
    },
    /// Consumption: ask the controller to cancel a job.
    JobCancel {
        #[command(flatten)]
        remote: Remote,
        id: String,
    },
}

/// Run one subcommand, writing what it prints to `out`.
pub async fn run(command: RemoteCommand, out: &mut dyn Write) -> Result<()> {
    match command {
        RemoteCommand::Admin(command) => admin(command, out).await,
        RemoteCommand::Jobs(command) => jobs(command, out).await,
    }
}

async fn admin(command: AdminCommand, out: &mut dyn Write) -> Result<()> {
    let value: Value = match command {
        AdminCommand::Status(remote) => remote.client()?.get("/api/v1/overview").await?,
        AdminCommand::EndpointAdd {
            remote,
            name,
            kind,
            base_url,
            api_key,
            public_url,
            data_destination,
        } => {
            let mut body = serde_json::to_value(CreateEndpoint {
                name,
                kind,
                base_url,
                api_key,
                managed_identity: Default::default(),
            })?;
            if let Some(public_url) = public_url {
                body["public_url"] = json!(public_url);
            }
            if let Some(destination) = data_destination {
                body["data_destination"] = json!(destination);
            }
            remote.client()?.post("/api/v1/endpoints", &body).await?
        }
        AdminCommand::ModelAdd(model) => {
            model
                .remote
                .client()?
                .post("/api/v1/model-profiles", &model)
                .await?
        }
        AdminCommand::ProjectAdd {
            remote,
            name,
            root,
            instruction_file,
        } => {
            remote
                .client()?
                .post(
                    "/api/v1/projects",
                    &CreateProject {
                        name,
                        roots: root,
                        instruction_files: instruction_file,
                    },
                )
                .await?
        }
        AdminCommand::NodeToken {
            remote,
            valid_minutes,
            node_name,
        } => {
            let mut body = json!({});
            if let Some(minutes) = valid_minutes {
                body["valid_minutes"] = json!(minutes);
            }
            if let Some(name) = node_name {
                body["node_name"] = json!(name);
            }
            remote
                .client()?
                .post("/api/v1/nodes/enrollment-tokens", &body)
                .await?
        }
        AdminCommand::NodeWol {
            remote,
            id,
            mac,
            broadcast,
        } => {
            let id = resource_id("machine", &id)?;
            remote
                .client()?
                .put(
                    &format!("/api/v1/nodes/{id}/wol"),
                    &json!({"mac": mac, "broadcast": broadcast}),
                )
                .await?
        }
        AdminCommand::NodeConfigExample(remote) => {
            let example: Value = remote.client()?.get("/api/v1/nodes/config-example").await?;
            let Some(toml) = example["toml"].as_str() else {
                bail!("the controller's config example has no toml field");
            };
            write!(out, "{toml}")?;
            return Ok(());
        }
    };
    print(out, &value)
}

async fn jobs(command: JobCommand, out: &mut dyn Write) -> Result<()> {
    match command {
        JobCommand::JobSubmit {
            remote,
            project_id,
            model_profile_id,
            harness_profile_id,
            prompt,
            idempotency_key,
            watch,
        } => {
            let client = remote.client()?;
            let idempotency_key = idempotency_key.unwrap_or_else(|| Uuid::new_v4().to_string());
            let job: Value = client
                .post(
                    "/api/v1/jobs",
                    &SubmitJob {
                        source: None,
                        idempotency_key: idempotency_key.clone(),
                        policy: Default::default(),
                        project_id,
                        model_profile_id,
                        harness_profile_id,
                        prompt,
                    },
                )
                .await?;
            print(out, &job)?;
            if watch {
                let id = submitted_job_id(&job, &idempotency_key)?;
                watch_job(&client, id, out).await?;
            }
            Ok(())
        }
        JobCommand::JobWatch { remote, id } => watch_job(&remote.client()?, &id, out).await,
        JobCommand::JobCancel { remote, id } => {
            let id = resource_id("job", &id)?;
            let value: Value = remote
                .client()?
                .post(&format!("/api/v1/jobs/{id}/cancel"), &json!({}))
                .await?;
            print(out, &value)
        }
    }
}

async fn watch_job(client: &ControllerClient, id: &str, out: &mut dyn Write) -> Result<()> {
    let job = client
        .follow_job(id, |event| {
            writeln!(out, "{}  {}", event.kind, event.message)?;
            Ok(())
        })
        .await?;
    print(out, &job)
}

/// Validate the submission response before watching it. An incompatible or
/// partial controller response must produce an actionable error, not a panic.
fn submitted_job_id<'a>(job: &'a Value, idempotency_key: &str) -> Result<&'a str> {
    let recovery = format!(
        "the job may already exist; rerun the same submission with \
         --idempotency-key {idempotency_key} to recover it without creating a duplicate"
    );
    let Some(fields) = job.as_object() else {
        bail!("controller returned a non-object job submission response; {recovery}");
    };
    match fields.get("id").and_then(Value::as_str) {
        Some(id) => match resource_id("job", id) {
            Ok(id) => Ok(id),
            Err(_) => bail!("controller returned an invalid job id; {recovery}"),
        },
        None => bail!("controller job submission response has no string id; {recovery}"),
    }
}

fn print(out: &mut dyn Write, value: &Value) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
    Ok(())
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
