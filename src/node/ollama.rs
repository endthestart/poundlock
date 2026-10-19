//! Node-local Ollama lifecycle. Requests never accept a URL or executable from
//! the controller. Only previously installed models are loaded.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    /// Off unless node.toml says `[ollama] enabled = true` (L10): otherwise a
    /// controller could load any installed model with any context.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_url")]
    pub base_url: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_url(),
        }
    }
}
fn default_url() -> String {
    "http://127.0.0.1:11434".into()
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.base_url)?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "Ollama must use a loopback HTTP address without credentials or a path"
        );
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Owned {
    #[serde(default)]
    source_model: String,
    context: u64,
    idle_seconds: u64,
    #[serde(skip)]
    leases: HashMap<String, Instant>,
    #[serde(skip)]
    idle_since: Option<Instant>,
    #[serde(skip)]
    last_error: Option<String>,
}

pub(super) struct Manager {
    config: Config,
    client: reqwest::Client,
    path: PathBuf,
    owned: HashMap<String, Owned>,
    report: Value,
    boot_id: String,
}
impl Manager {
    pub fn open(config: Config, data: &Path) -> Result<Self> {
        config.validate()?;
        let path = data.join("ollama-owned.json");
        let mut owned: HashMap<String, Owned> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Read Ollama ownership ledger")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e.into()),
        };
        // A restarted node has no confirmed leases. Clean up only its ledger's
        // models before accepting work, never arbitrary models returned by /ps.
        for entry in owned.values_mut() {
            entry.idle_seconds = 0;
            entry.idle_since = Some(Instant::now());
        }
        Ok(Self {
            config,
            path,
            owned,
            report: json!({"status":"checking","models":[]}),
            boot_id: uuid::Uuid::new_v4().to_string(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(120))
                .build()?,
        })
    }
    fn save(&self) -> Result<()> {
        use std::io::Write;
        let parent = self
            .path
            .parent()
            .context("Ollama ledger directory missing")?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, &self.owned)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        Ok(())
    }
    pub fn report(&self) -> Value {
        let mut report = self.report.clone();
        report["boot_id"] = json!(self.boot_id);
        report
    }
    pub fn busy(&self) -> bool {
        !self.owned.is_empty()
    }
    async fn running(&self) -> Result<Vec<Value>> {
        let value: Value = self
            .client
            .get(format!(
                "{}/api/ps",
                self.config.base_url.trim_end_matches('/')
            ))
            .timeout(Duration::from_secs(3))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        value["models"]
            .as_array()
            .cloned()
            .context("Ollama did not return a loaded-model list")
    }
    async fn installed(&self) -> Result<Vec<Value>> {
        let value: Value = self
            .client
            .get(format!(
                "{}/api/tags",
                self.config.base_url.trim_end_matches('/')
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        value["models"]
            .as_array()
            .cloned()
            .context("Ollama did not return an installed-model list")
    }
    async fn context_model(&self, model: &str, context: u64) -> Result<String> {
        let installed = self.installed().await?;
        let source = installed
            .iter()
            .find(|candidate| model_name(candidate) == Some(model))
            .with_context(|| format!("Ollama model '{model}' is not installed"))?;
        let digest = source["digest"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .context("Installed Ollama model has no usable digest")?;
        let fingerprint = crate::crypto::sha256_hex(format!("{digest}\0{context}").as_bytes());
        let runtime_model = format!("poundlock-context-{}:latest", &fingerprint[..24]);
        if installed
            .iter()
            .any(|candidate| model_name(candidate) == Some(runtime_model.as_str()))
        {
            return Ok(runtime_model);
        }
        let value: Value = self
            .client
            .post(format!(
                "{}/api/create",
                self.config.base_url.trim_end_matches('/')
            ))
            .json(&json!({
                "model": runtime_model,
                "from": model,
                "parameters": {"num_ctx": context},
                "stream": false
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            value["status"] == "success",
            "Ollama did not create the managed context model"
        );
        Ok(runtime_model)
    }
    async fn retain(&self, model: &str, context: u64, seconds: u64) -> Result<()> {
        let mut body = json!({"model":model,"stream":false,"keep_alive":seconds});
        if seconds > 0 {
            body["options"] = json!({"num_ctx":context});
        }
        let value: Value = self
            .client
            .post(format!(
                "{}/api/generate",
                self.config.base_url.trim_end_matches('/')
            ))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            value.get("error").is_none() && value["done"] == true,
            "Ollama did not finish the model-memory operation"
        );
        Ok(())
    }
    pub async fn action(&mut self, kind: &str, lease: &str, payload: &Value) -> Result<Value> {
        // Release and stop touch only models this node's ledger owns, so they
        // still run after Ollama is turned off.
        ensure!(
            self.config.enabled || kind != "ensure",
            "Ollama is not enabled on this node; add [ollama] enabled = true to node.toml"
        );
        let mut resolved_model_id = None;
        if kind == "ensure" {
            let source_model = payload["model_id"]
                .as_str()
                .context("Ollama model ID is missing")?;
            ensure!(
                !source_model.is_empty()
                    && source_model.len() <= 256
                    && !source_model.chars().any(char::is_control),
                "Invalid Ollama model ID"
            );
            let context = payload["context_tokens"]
                .as_u64()
                .context("Ollama context limit is missing")?;
            let idle = payload["idle_seconds"].as_u64().unwrap_or(0);
            ensure!(
                (1024..=1048576).contains(&context) && idle <= 3600,
                "Invalid model memory settings"
            );
            let model = self.context_model(source_model, context).await?;
            resolved_model_id = Some(model.clone());
            ensure!(
                !self
                    .owned
                    .iter()
                    .any(|(name, entry)| name != &model && !entry.leases.is_empty()),
                "Another Ollama model has active work"
            );
            // Refuse to adopt a model owned by a separate application.
            let running = self.running().await?;
            ensure!(
                !running.iter().any(|m| {
                    matches!(
                        model_name(m),
                        Some(name) if name == source_model || name == model
                    )
                }) || self.owned.contains_key(&model),
                "This model was loaded outside Poundlock. Stop it in Ollama before enabling managed loading"
            );
            for (name, entry) in self.owned.iter_mut() {
                if name != &model {
                    entry.idle_seconds = 0;
                    entry.idle_since = Some(Instant::now());
                }
            }
            self.cleanup().await?;
            let entry = self.owned.entry(model.clone()).or_default();
            ensure!(
                entry.leases.is_empty() || (entry.context == context && entry.idle_seconds == idle),
                "Active Ollama work uses different memory settings"
            );
            entry.source_model = source_model.into();
            entry.context = context;
            entry.idle_seconds = idle;
            entry.idle_since = None;
            entry.leases.insert(lease.into(), Instant::now());
            // Record ownership before loading so restart can recover it.
            self.save()?;
            if let Err(error) = self.retain(&model, context, 300).await {
                self.end(lease, true);
                let _ = self.cleanup().await;
                return Err(error);
            }
        } else if kind == "renew" {
            if let Some((model, entry)) = self
                .owned
                .iter_mut()
                .find(|(_, entry)| entry.leases.contains_key(lease))
            {
                entry.leases.insert(lease.into(), Instant::now());
                let (model, context) = (model.clone(), entry.context);
                self.retain(&model, context, 300).await?;
            }
        } else if matches!(kind, "release" | "stop") {
            self.end(lease, kind == "stop");
            self.save()?;
            let warm: Vec<_> = self
                .owned
                .iter()
                .filter(|(_, v)| v.leases.is_empty() && v.idle_seconds > 0)
                .map(|(k, v)| (k.clone(), v.context, v.idle_seconds))
                .collect();
            for (model, context, seconds) in warm {
                self.retain(&model, context, seconds).await?;
            }
            self.cleanup().await?;
        }
        self.observe().await;
        let mut report = self.report();
        if let Some(model) = resolved_model_id {
            report["resolved_model_id"] = json!(model);
        }
        Ok(report)
    }
    fn end(&mut self, lease: &str, force: bool) {
        for entry in self.owned.values_mut() {
            if entry.leases.remove(lease).is_some() && entry.leases.is_empty() {
                if force {
                    entry.idle_seconds = 0;
                }
                entry.idle_since = Some(Instant::now());
            }
        }
    }
    async fn cleanup(&mut self) -> Result<()> {
        let due: Vec<_> = self
            .owned
            .iter()
            .filter(|(_, v)| {
                v.leases.is_empty()
                    && v.idle_since
                        .is_some_and(|t| t.elapsed() >= Duration::from_secs(v.idle_seconds))
            })
            .map(|(k, v)| (k.clone(), v.context))
            .collect();
        let mut failed = None;
        for (model, context) in due {
            let result = async {
                self.retain(&model, context, 0).await?;
                // A successful HTTP response alone does not prove memory is free.
                ensure!(
                    !self
                        .running()
                        .await?
                        .iter()
                        .any(|m| model_name(m) == Some(model.as_str())),
                    "Ollama still reports this model loaded"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            match result {
                Ok(()) => {
                    self.owned.remove(&model);
                    self.save()?;
                }
                Err(e) => {
                    self.owned.get_mut(&model).unwrap().last_error = Some(e.to_string());
                    failed = Some(e);
                }
            }
        }
        if let Some(error) = failed {
            return Err(error);
        }
        Ok(())
    }
    pub async fn tick(&mut self, timeout: Duration) {
        if !self.config.enabled && self.owned.is_empty() {
            self.report = json!({"status":"disabled","models":[]});
            return;
        }
        let stale: Vec<_> = self
            .owned
            .values()
            .flat_map(|v| {
                v.leases
                    .iter()
                    .filter(|(_, t)| t.elapsed() >= timeout)
                    .map(|(k, _)| k.clone())
            })
            .collect();
        for lease in stale {
            self.end(&lease, true);
        }
        if let Err(error) = self.cleanup().await {
            tracing::warn!(%error,"Ollama cleanup pending");
        }
        self.observe().await;
    }
    async fn observe(&mut self) {
        match self.running().await {
            Ok(models) => {
                let mut rows:Vec<Value> = models.iter().map(|m| {
                    let name=model_name(m).unwrap_or("unknown");let owned=self.owned.get(name);
                    json!({"model":owned.filter(|o|!o.source_model.is_empty()).map_or(name,|o|o.source_model.as_str()),"runtime_model":name,"loaded":true,"managed":owned.is_some(),"active_leases":owned.map_or(0,|o|o.leases.len()),"idle_seconds":owned.map(|o|o.idle_seconds),"context_tokens":m["context_length"],"gpu_bytes":m["size_vram"],"error":owned.and_then(|o|o.last_error.as_ref())})
                }).collect();
                for (name, o) in &self.owned {
                    if !models.iter().any(|m| model_name(m) == Some(name.as_str())) {
                        rows.push(json!({"model":if o.source_model.is_empty(){name}else{&o.source_model},"runtime_model":name,"loaded":false,"managed":true,"active_leases":o.leases.len(),"idle_seconds":o.idle_seconds,"context_tokens":o.context,"error":o.last_error}));
                    }
                }
                self.report = json!({"status":"observed","checked_at":chrono::Utc::now().to_rfc3339(),"models":rows});
            }
            Err(error) => {
                self.report["status"] = json!("unavailable");
                self.report["error"] = json!(error.to_string());
            }
        }
    }
}
fn model_name(value: &Value) -> Option<&str> {
    value
        .get("name")
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        routing::{get, post},
    };
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Server {
        installed: HashMap<String, String>,
        loaded: HashMap<String, u64>,
        requests: Vec<Value>,
        fail_unload: bool,
    }
    struct Fixture {
        server: Arc<Mutex<Server>>,
        task: tokio::task::JoinHandle<()>,
        dir: tempfile::TempDir,
        manager: Manager,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn fixture() -> Fixture {
        let server = Arc::new(Mutex::new(Server {
            installed: HashMap::from([("coding:4b".into(), "source-digest".into())]),
            ..Server::default()
        }));
        let app=Router::new().route("/api/ps",get(|State(s):State<Arc<Mutex<Server>>>|async move {
            let s=s.lock().unwrap();Json(json!({"models":s.loaded.keys().map(|name|json!({"name":name,"size_vram":1024,"context_length":4096})).collect::<Vec<_>>()}))
        })).route("/api/tags",get(|State(s):State<Arc<Mutex<Server>>>|async move {
            let s=s.lock().unwrap();Json(json!({"models":s.installed.iter().map(|(name,digest)|json!({"name":name,"digest":digest})).collect::<Vec<_>>()}))
        })).route("/api/create",post(|State(s):State<Arc<Mutex<Server>>>,Json(body):Json<Value>|async move {
            let mut s=s.lock().unwrap();s.requests.push(body.clone());
            let name=body["model"].as_str().unwrap().to_string();
            let digest=crate::crypto::sha256_hex(body.to_string().as_bytes());
            s.installed.insert(name,digest);Json(json!({"status":"success"}))
        })).route("/api/generate",post(|State(s):State<Arc<Mutex<Server>>>,Json(body):Json<Value>|async move {
            let mut s=s.lock().unwrap();s.requests.push(body.clone());
            let model=body["model"].as_str().unwrap().to_string();let seconds=body["keep_alive"].as_u64().unwrap();
            if seconds==0 && s.fail_unload {return Json(json!({"error":"unload failed"}));}
            if seconds==0 {s.loaded.remove(&model);}else{s.loaded.insert(model,seconds);}
            Json(json!({"done":true}))
        })).with_state(server.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let manager = Manager::open(
            Config {
                enabled: true,
                base_url: format!("http://{addr}"),
            },
            dir.path(),
        )
        .unwrap();
        Fixture {
            server,
            task,
            dir,
            manager,
        }
    }
    fn input(idle: u64) -> Value {
        json!({"model_id":"coding:4b","context_tokens":4096,"idle_seconds":idle})
    }
    fn managed_model(context: u64) -> String {
        let fingerprint = crate::crypto::sha256_hex(format!("source-digest\0{context}").as_bytes());
        format!("poundlock-context-{}:latest", &fingerprint[..24])
    }
    #[tokio::test]
    async fn ensure_creates_and_returns_a_context_specific_model() {
        let mut f = fixture().await;
        let mut payload = input(0);
        payload["context_tokens"] = json!(32768);
        let result = f.manager.action("ensure", "a", &payload).await.unwrap();
        let model = managed_model(32768);
        assert_eq!(result["resolved_model_id"], model);
        let requests = f.server.lock().unwrap().requests.clone();
        assert_eq!(requests[0]["from"], "coding:4b");
        assert_eq!(requests[0]["parameters"]["num_ctx"], 32768);
        assert_eq!(requests[1]["model"], model);
        f.manager.action("release", "a", &json!({})).await.unwrap();
    }
    #[tokio::test]
    async fn active_work_survives_tool_gaps_and_last_release_unloads() {
        let mut f = fixture().await;
        f.manager.action("renew", "late", &json!({})).await.unwrap();
        assert!(f.server.lock().unwrap().requests.is_empty());
        f.manager.action("ensure", "a", &input(0)).await.unwrap();
        f.manager.action("ensure", "b", &input(0)).await.unwrap();
        f.manager.action("release", "a", &json!({})).await.unwrap();
        f.manager.action("renew", "b", &json!({})).await.unwrap();
        assert_eq!(f.server.lock().unwrap().loaded.len(), 1);
        f.manager.action("release", "b", &json!({})).await.unwrap();
        assert!(f.server.lock().unwrap().loaded.is_empty());
        f.manager.action("renew", "b", &json!({})).await.unwrap();
        assert!(f.server.lock().unwrap().loaded.is_empty());
        assert!(!f.manager.busy());
    }
    #[tokio::test]
    async fn idle_policy_expires_and_failed_unload_remains_owned_for_retry() {
        let mut f = fixture().await;
        f.manager.action("ensure", "a", &input(60)).await.unwrap();
        f.manager.action("release", "a", &json!({})).await.unwrap();
        let model = managed_model(4096);
        assert_eq!(f.server.lock().unwrap().loaded[&model], 60);
        f.manager.owned.get_mut(&model).unwrap().idle_since =
            Some(Instant::now() - Duration::from_secs(61));
        f.server.lock().unwrap().fail_unload = true;
        f.manager.tick(Duration::from_secs(300)).await;
        assert!(f.manager.report()["models"][0]["error"].is_string());
        assert!(f.manager.busy());
        f.server.lock().unwrap().fail_unload = false;
        f.manager.tick(Duration::from_secs(300)).await;
        assert!(!f.manager.busy());
        assert!(f.server.lock().unwrap().loaded.is_empty());
    }
    #[tokio::test]
    async fn lost_lease_and_restart_clean_only_owned_models() {
        let mut f = fixture().await;
        f.server
            .lock()
            .unwrap()
            .loaded
            .insert("outside:8b".into(), 300);
        f.server
            .lock()
            .unwrap()
            .installed
            .insert("outside:8b".into(), "outside-digest".into());
        let outside = json!({"model_id":"outside:8b","context_tokens":4096});
        assert!(
            f.manager
                .action("ensure", "outside", &outside)
                .await
                .is_err()
        );
        f.manager.action("ensure", "a", &input(3600)).await.unwrap();
        f.manager
            .owned
            .get_mut(&managed_model(4096))
            .unwrap()
            .leases
            .insert("a".into(), Instant::now() - Duration::from_secs(301));
        f.manager.tick(Duration::from_secs(300)).await;
        assert!(!f.manager.busy());
        assert_eq!(f.server.lock().unwrap().loaded.len(), 1);
        f.manager
            .action("ensure", "new", &input(3600))
            .await
            .unwrap();
        let mut restarted = Manager::open(f.manager.config.clone(), f.dir.path()).unwrap();
        restarted.tick(Duration::from_secs(300)).await;
        assert!(!restarted.busy());
        assert_eq!(f.server.lock().unwrap().loaded.len(), 1);
        assert!(f.server.lock().unwrap().loaded.contains_key("outside:8b"));
    }
    #[tokio::test]
    async fn conflicting_model_and_policy_cannot_replace_active_work() {
        let mut f = fixture().await;
        f.manager.action("ensure", "a", &input(0)).await.unwrap();
        assert!(f.manager.action("ensure", "b", &input(60)).await.is_err());
        let mut other = input(0);
        other["model_id"] = json!("other:8b");
        assert!(f.manager.action("ensure", "b", &other).await.is_err());
        f.manager.action("stop", "a", &json!({})).await.unwrap();
        assert!(f.server.lock().unwrap().loaded.is_empty());
    }
    #[test]
    fn adapter_cannot_be_pointed_at_remote_services() {
        for value in [
            "http://example.com:11434",
            "http://127.0.0.1:11434/api",
            "http://user:secret@127.0.0.1:11434",
            "http://127.0.0.1:11434?target=x",
        ] {
            assert!(
                Config {
                    enabled: true,
                    base_url: value.into()
                }
                .validate()
                .is_err()
            );
        }
        Config::default().validate().unwrap();
    }

    #[tokio::test]
    async fn a_disabled_ollama_refuses_ensure_and_is_not_polled() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing listens here: a disabled manager must not try.
        let mut manager = Manager::open(Config::default(), dir.path()).unwrap();
        manager.tick(Duration::from_secs(300)).await;
        assert_eq!(manager.report()["status"], "disabled");
        let error = manager
            .action("ensure", "lease", &json!({"model_id":"qwen"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("enabled = true"), "{error}");
    }
}
