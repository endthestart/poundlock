//! Read-only, best-effort inventory. Never enumerate personal files, start a
//! model server, contact an inference endpoint, or return raw command output.
use serde_json::{Value, json};
use std::collections::HashMap;
#[cfg(any(target_os = "linux", test))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(super) const REFRESH_SECONDS: u64 = 300;

// Identity probes are host-native software Poundlock does not manage; keep
// their footprint small and bounded so a slow or hostile command cannot stall
// a heartbeat or grow the inventory without limit.
const IDENTITY_TIMEOUT_SECONDS: u64 = 10;
const IDENTITY_OUTPUT_CAP: usize = 64 * 1024;

pub(super) async fn collect(data_dir: &Path, identities: &HashMap<String, Vec<String>>) -> Value {
    let mut inventory = json!({
        "schema_version":1, "collected_at":chrono::Utc::now().to_rfc3339(),
        "os":{"name":std::env::consts::OS,"arch":std::env::consts::ARCH},
        "cpu":{"logical_cores":std::thread::available_parallelism().ok().map(|n|n.get())},
        "memory":{}, "gpus":[], "software":software(),
        "storage":storage(data_dir), "limitations":[],
        "runtimes":identity_report(identities).await
    });
    #[cfg(target_os = "linux")]
    {
        linux(&mut inventory);
        // lspci supplies marketing names when pciutils is installed. PCI IDs
        // and driver details remain available through sysfs without it.
        if let Some(raw) = probe("lspci", &["-Dmmnn"]).await {
            for gpu in inventory["gpus"].as_array_mut().unwrap() {
                if let Some(line) = raw
                    .lines()
                    .find(|line| line.split_whitespace().next() == gpu["pci_address"].as_str())
                {
                    gpu["name"] = json!(line.split('"').nth(5).map(clean));
                }
            }
        }
        if inventory["gpus"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["vendor_id"] == "0x10de")
            && let Some(raw) = probe(
                "nvidia-smi",
                &[
                    "--query-gpu=pci.bus_id,name,memory.total,driver_version",
                    "--format=csv,noheader,nounits",
                ],
            )
            .await
        {
            for line in raw.lines() {
                let parts: Vec<_> = line.split(',').map(str::trim).collect();
                if parts.len() != 4 {
                    continue;
                }
                for gpu in inventory["gpus"].as_array_mut().unwrap() {
                    if gpu["vendor_id"] == "0x10de"
                        && gpu["pci_address"]
                            .as_str()
                            .is_some_and(|pci| parts[0].to_lowercase().ends_with(pci))
                    {
                        gpu["name"] = json!(clean(parts[1]));
                        gpu["vram_bytes"] = json!(
                            parts[2]
                                .parse::<u64>()
                                .ok()
                                .and_then(|n| n.checked_mul(1024 * 1024))
                        );
                        gpu["driver_version"] = json!(clean(parts[3]));
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(version) = probe("sw_vers", &["-productVersion"]).await {
            inventory["os"]["version"] = json!(clean(&version));
        }
        for (key, section, field) in [
            ("machdep.cpu.brand_string", "cpu", "model"),
            ("hw.memsize", "memory", "total_bytes"),
            ("hw.model", "os", "machine_model"),
            ("kern.osrelease", "os", "kernel"),
        ] {
            if let Some(value) = probe("sysctl", &["-n", key]).await {
                inventory[section][field] = if field.ends_with("_bytes") {
                    json!(value.trim().parse::<u64>().ok())
                } else {
                    json!(clean(&value))
                };
            }
        }
        if let Some(raw) = probe("system_profiler", &["SPDisplaysDataType", "-json"]).await
            && let Ok(data) = serde_json::from_str::<Value>(&raw)
            && let Some(displays) = data["SPDisplaysDataType"].as_array()
        {
            inventory["gpus"] = json!(displays.iter().take(32).map(|gpu|json!({
                        "name":gpu["sppci_model"].as_str().map(clean),
                        "vendor":gpu["spdisplays_vendor"].as_str().map(clean),
                        "vram":gpu["spdisplays_vram"].as_str().or(gpu["spdisplays_vram_shared"].as_str()).map(clean),
                        "metal":gpu["spdisplays_metal"].as_str().map(clean)
                    })).collect::<Vec<_>>());
        }
    }
    let mut limitations = vec![
        "Software detection checks standard installation locations; custom paths and container contents may not be detected.",
    ];
    if inventory["gpus"].as_array().is_none_or(Vec::is_empty) {
        limitations.push("GPU details unavailable to this service account; this does not establish that no GPU is installed.");
    }
    limitations
        .push("Installed software and GPU detection do not verify inference or wake support.");
    inventory["limitations"] = json!(limitations);
    inventory
}

fn clean(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn read(path: impl AsRef<Path>) -> Option<String> {
    let mut raw = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(256 * 1024)
        .read_to_string(&mut raw)
        .ok()?;
    Some(raw)
}

#[cfg(any(target_os = "linux", test))]
fn text(path: impl AsRef<Path>) -> Option<String> {
    read(path).map(|s| clean(&s))
}

// Use known system locations, not a working directory or an inherited PATH.
fn executable(name: &str) -> Option<PathBuf> {
    [
        "/usr/bin",
        "/usr/sbin",
        "/bin",
        "/sbin",
        "/usr/local/bin",
        "/opt/homebrew/bin",
    ]
    .iter()
    .map(|dir| Path::new(dir).join(name))
    .find(|p| p.is_file())
}

async fn probe(name: &str, args: &[&str]) -> Option<String> {
    let mut child = tokio::process::Command::new(executable(name)?)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(256 * 1024 + 1);
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        let mut raw = String::new();
        stdout.read_to_string(&mut raw).await.ok()?;
        if raw.len() > 256 * 1024 {
            return None;
        }
        child.wait().await.ok()?.success().then_some(raw)
    })
    .await
    .ok()
    .flatten();
    if result.is_none() {
        let _ = child.kill().await;
    }
    result
}

/// Runs each configured `identity` command and returns a sanitised, bounded
/// report keyed by runtime name. Runtimes without an `identity` command are
/// left out entirely; a command that fails, times out, or returns something
/// unexpected reports `{"error": "<short code>"}` for that runtime only —
/// never a crash and never a delayed heartbeat.
/// The runtime's identity probe, run once right now, for a caller that needs
/// what is serving at this moment rather than the last periodic report.
pub(super) async fn runtime_identity(command: &[String]) -> Option<Value> {
    if command.is_empty() {
        return None;
    }
    Some(run_identity(command, Duration::from_secs(IDENTITY_TIMEOUT_SECONDS)).await)
}

async fn identity_report(identities: &HashMap<String, Vec<String>>) -> Value {
    let mut runtimes = serde_json::Map::new();
    for (name, command) in identities {
        if command.is_empty() {
            continue;
        }
        runtimes.insert(
            name.clone(),
            run_identity(command, Duration::from_secs(IDENTITY_TIMEOUT_SECONDS)).await,
        );
    }
    Value::Object(runtimes)
}

async fn run_identity(command: &[String], timeout: Duration) -> Value {
    let mut process = tokio::process::Command::new(&command[0]);
    process
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::process::isolate(&mut process);
    let Ok(mut child) = process.spawn() else {
        return json!({"error": "spawn_failed"});
    };
    // Armed until a clean, well-formed exit proves nothing was left running;
    // a timeout, oversized output, or failure keeps it armed so Drop kills
    // the whole process group rather than leaking a stray server.
    let Ok(mut group) = crate::process::ProcessGroup::new(&child) else {
        return json!({"error": "spawn_failed"});
    };
    let Some(stdout) = child.stdout.take() else {
        return json!({"error": "spawn_failed"});
    };
    let mut stdout = stdout.take(IDENTITY_OUTPUT_CAP as u64 + 1);
    let outcome = tokio::time::timeout(timeout, async {
        let mut raw = Vec::new();
        stdout.read_to_end(&mut raw).await.ok()?;
        if raw.len() > IDENTITY_OUTPUT_CAP {
            return None;
        }
        child.wait().await.ok()?.success().then_some(raw)
    })
    .await;
    match outcome {
        Ok(Some(raw)) => {
            group.disarm();
            match serde_json::from_slice::<Value>(&raw) {
                Ok(Value::Object(object)) => sanitize_identity(&object),
                _ => json!({"error": "invalid_output"}),
            }
        }
        Ok(None) => json!({"error": "exec_failed"}),
        Err(_) => json!({"error": "timeout"}),
    }
}

// Drop everything but a small, conservatively-charactered allowlist. Values
// that are missing, the wrong type, empty, or too long are left out rather
// than truncated, so a partial report never looks like a complete one.
fn sanitize_identity(object: &serde_json::Map<String, Value>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(application) = object.get("application").and_then(Value::as_object) {
        let mut clean_application = serde_json::Map::new();
        for field in ["name", "version"] {
            if let Some(value) = application.get(field).and_then(Value::as_str)
                && is_clean(value, 80)
            {
                clean_application.insert(field.into(), json!(value));
            }
        }
        if !clean_application.is_empty() {
            out.insert("application".into(), Value::Object(clean_application));
        }
    }
    if let Some(build) = object.get("build").and_then(Value::as_str)
        && is_clean(build, 200)
    {
        out.insert("build".into(), json!(build));
    }
    if let Some(commit) = object.get("commit").and_then(Value::as_str)
        && is_hex(commit, 40)
    {
        out.insert("commit".into(), json!(commit));
    }
    if let Some(model) = object.get("model").and_then(Value::as_str)
        && is_clean(model, 200)
    {
        out.insert("model".into(), json!(model));
    }
    // Model ids this runtime can serve, so the controller can refuse a profile
    // whose model no machine could ever load.
    if let Some(servable) = object.get("servable").and_then(Value::as_array) {
        let ids: Vec<&str> = servable
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| is_clean(id, 200))
            .take(32)
            .collect();
        if !ids.is_empty() {
            out.insert("servable".into(), json!(ids));
        }
    }
    // What each servable model is, so the controller can offer it with its
    // context instead of asking for it by hand. Older runtimes omit it.
    if let Some(models) = object.get("models").and_then(Value::as_array) {
        let models: Vec<Value> = models.iter().filter_map(sanitize_model).take(32).collect();
        if !models.is_empty() {
            out.insert("models".into(), json!(models));
        }
    }
    out.insert("checked_at".into(), json!(chrono::Utc::now().to_rfc3339()));
    Value::Object(out)
}

/// One `models` entry: an id is required; a label or context that fails the
/// same checks is dropped from the entry, not the whole report.
fn sanitize_model(model: &Value) -> Option<Value> {
    let id = model.get("id").and_then(Value::as_str)?;
    if !is_clean(id, 200) {
        return None;
    }
    let mut clean = serde_json::Map::new();
    clean.insert("id".into(), json!(id));
    if let Some(label) = model.get("label").and_then(Value::as_str)
        && is_clean(label, 200)
    {
        clean.insert("label".into(), json!(label));
    }
    if let Some(context) = model.get("context_tokens").and_then(Value::as_u64)
        && (1..=16_777_216).contains(&context)
    {
        clean.insert("context_tokens".into(), json!(context));
    }
    Some(Value::Object(clean))
}

fn is_clean(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.chars().count() <= max_len
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_' | '+' | ':' | '(' | ')')
        })
}

fn is_hex(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn software() -> Vec<Value> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let programs = [
        ("Ollama", "ollama", ""),
        ("LM Studio CLI", "lms", ".lmstudio/bin/lms"),
        #[cfg(target_os = "macos")]
        ("oMLX", "omlx", ".omlx/bin/omlx"),
        ("llama.cpp server", "llama-server", ""),
        #[cfg(target_os = "linux")]
        ("vLLM", "vllm", ".local/bin/vllm"),
        #[cfg(target_os = "linux")]
        ("SGLang", "sglang", ".local/bin/sglang"),
        ("Docker", "docker", ""),
        ("Podman", "podman", ""),
        #[cfg(target_os = "linux")]
        ("Vulkan tools", "vulkaninfo", ""),
        #[cfg(target_os = "linux")]
        ("ROCm tools", "rocminfo", ""),
        #[cfg(target_os = "linux")]
        ("NVIDIA tools", "nvidia-smi", ""),
    ];
    let mut software: Vec<_> = programs.iter().map(|(name, program, local)| {
        let found = executable(program).is_some() || (!local.is_empty() && home.as_ref().is_some_and(|h|h.join(local).is_file()));
        json!({"name":name,"executable":program,"status":if found {"detected"} else {"not_detected"}})
    }).collect();
    #[cfg(target_os = "macos")]
    for app in ["LM Studio", "oMLX", "ToshLLM"] {
        let found = Path::new("/Applications")
            .join(format!("{app}.app"))
            .is_dir()
            || home
                .as_ref()
                .is_some_and(|h| h.join(format!("Applications/{app}.app")).is_dir());
        software.push(json!({"name":format!("{app} desktop"),"status":if found {"detected"} else {"not_detected"}}));
    }
    #[cfg(target_os = "linux")]
    for (name, paths) in [
        (
            "ROCm HIP library",
            ["/usr/lib/libamdhip64.so", "/opt/rocm/lib/libamdhip64.so"],
        ),
        (
            "Vulkan loader",
            [
                "/usr/lib/libvulkan.so.1",
                "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
            ],
        ),
    ] {
        software.push(json!({"name":name,"status":if paths.iter().any(|p|Path::new(p).is_file()) {"detected"} else {"not_detected"}}));
    }
    software
}

// libc uses different integer widths for these fields on macOS and Linux.
#[allow(clippy::unnecessary_cast)]
fn storage(path: &Path) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
            let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            // SAFETY: path is NUL terminated and statvfs writes the supplied struct.
            if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } == 0 {
                let info = unsafe { info.assume_init() };
                return json!({"scope":"node data filesystem","total_bytes":(info.f_blocks as u64).checked_mul(info.f_frsize as u64),"available_bytes":(info.f_bavail as u64).checked_mul(info.f_frsize as u64)});
            }
        }
    }
    json!({"scope":"node data filesystem","status":"unavailable"})
}

#[cfg(target_os = "linux")]
fn linux(inventory: &mut Value) {
    let release = read("/etc/os-release")
        .or_else(|| read("/usr/lib/os-release"))
        .unwrap_or_default();
    for (source, field) in [
        ("ID", "id"),
        ("ID_LIKE", "family"),
        ("PRETTY_NAME", "name"),
        ("VERSION_ID", "version"),
    ] {
        inventory["os"][field] = json!(release_value(&release, source));
    }
    inventory["os"]["kernel"] = json!(text("/proc/sys/kernel/osrelease"));
    inventory["os"]["machine_vendor"] = json!(text("/sys/class/dmi/id/sys_vendor"));
    inventory["os"]["machine_model"] = json!(text("/sys/class/dmi/id/product_name"));
    inventory["os"]["glibc"] = json!(glibc_version());
    if let Some(cpu) = read("/proc/cpuinfo") {
        inventory["cpu"]["model"] = json!(
            cpu.lines()
                .filter_map(|s| s.split_once(':'))
                .find(|(k, _)| matches!(k.trim(), "model name" | "Hardware"))
                .map(|(_, v)| clean(v))
        );
    }
    if let Some(memory) = read("/proc/meminfo") {
        for (source, field) in [
            ("MemTotal", "total_bytes"),
            ("MemAvailable", "available_bytes"),
            ("SwapTotal", "swap_total_bytes"),
        ] {
            inventory["memory"][field] = json!(memory_bytes(&memory, source));
        }
    }
    inventory["gpus"] = json!(linux_gpus(Path::new("/sys/bus/pci/devices")));
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn glibc_version() -> Option<String> {
    unsafe extern "C" {
        fn gnu_get_libc_version() -> *const libc::c_char;
    }
    // SAFETY: glibc returns a static NUL-terminated version string.
    Some(
        unsafe { std::ffi::CStr::from_ptr(gnu_get_libc_version()) }
            .to_string_lossy()
            .into_owned(),
    )
}
#[cfg(all(target_os = "linux", not(target_env = "gnu")))]
fn glibc_version() -> Option<String> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn release_value(raw: &str, key: &str) -> Option<String> {
    raw.lines()
        .filter_map(|s| s.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| clean(v.trim().trim_matches(['\'', '"'])))
}

#[cfg(any(target_os = "linux", test))]
fn memory_bytes(raw: &str, key: &str) -> Option<u64> {
    raw.lines()
        .filter_map(|s| s.split_once(':'))
        .find(|(k, _)| *k == key)?
        .1
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

#[cfg(any(target_os = "linux", test))]
fn linux_gpus(root: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return vec![];
    };
    let mut gpus = vec![];
    for entry in entries.flatten().take(4096) {
        let path = entry.path();
        if !text(path.join("class")).is_some_and(|c| c.starts_with("0x03")) {
            continue;
        }
        let vendor = text(path.join("vendor"));
        let device = text(path.join("device"));
        let vendor_name = match vendor.as_deref() {
            Some("0x1002") => "AMD",
            Some("0x10de") => "NVIDIA",
            Some("0x8086") => "Intel",
            _ => "Other",
        };
        gpus.push(json!({"vendor":vendor_name,"vendor_id":vendor,"device_id":device,
            "pci_address":entry.file_name().to_string_lossy(),
            "name":text(path.join("product_name")),
            "driver":std::fs::read_link(path.join("driver")).ok().and_then(|p|p.file_name().map(|s|s.to_string_lossy().into_owned())),
            "vram_bytes":text(path.join("mem_info_vram_total")).and_then(|s|s.parse::<u64>().ok())}));
        if gpus.len() == 32 {
            break;
        }
    }
    gpus.sort_by_key(|gpu| gpu["pci_address"].as_str().unwrap_or_default().to_owned());
    gpus
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_inventory_uses_allowlisted_fields_and_reports_gpu_without_optional_tools() {
        let root = tempfile::tempdir().unwrap();
        let gpu = root.path().join("0000:03:00.0");
        std::fs::create_dir(&gpu).unwrap();
        for (name, value) in [
            ("class", "0x030000"),
            ("vendor", "0x1002"),
            ("device", "0x7550"),
            ("mem_info_vram_total", "17179869184"),
            ("serial", "DO-NOT-COLLECT"),
        ] {
            std::fs::write(gpu.join(name), value).unwrap();
        }
        let inventory = linux_gpus(root.path());
        assert_eq!(inventory[0]["vendor"], "AMD");
        assert_eq!(inventory[0]["vram_bytes"], 17179869184u64);
        assert!(inventory[0]["name"].is_null());
        assert!(
            !serde_json::to_string(&inventory)
                .unwrap()
                .contains("DO-NOT-COLLECT")
        );
        assert_eq!(
            release_value(
                "ID=cachyos\nPRETTY_NAME=\"CachyOS Linux\"\nSECRET=hidden",
                "PRETTY_NAME"
            )
            .as_deref(),
            Some("CachyOS Linux")
        );
        assert_eq!(
            memory_bytes("MemTotal: 32768 kB\n", "MemTotal"),
            Some(33554432)
        );
        assert_eq!(memory_bytes("MemTotal: invalid\n", "MemTotal"), None);
    }
    #[tokio::test]
    async fn inventory_is_bounded_and_useful_without_model_server() {
        let root = tempfile::tempdir().unwrap();
        let inventory = collect(root.path(), &HashMap::new()).await;
        assert_eq!(inventory["schema_version"], 1);
        assert!(inventory["collected_at"].is_string());
        assert!(inventory["software"].is_array());
        assert!(inventory["runtimes"].as_object().unwrap().is_empty());
        assert!(inventory["storage"]["total_bytes"].as_u64().unwrap() > 0);
        assert!(serde_json::to_vec(&inventory).unwrap().len() < 65536);
        assert!(
            !inventory
                .to_string()
                .contains(root.path().to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn runtime_identity_probes_now_or_says_nothing() {
        // Runs after a successful ensure, so provenance can name the model that
        // just loaded instead of the five-minute-old inventory's.
        assert!(runtime_identity(&[]).await.is_none());
        let command = vec![
            "sh".into(),
            "-c".into(),
            r#"printf '%s' '{"model":"Qwen3.8-27B"}'"#.into(),
        ];
        let identity = runtime_identity(&command).await.unwrap();
        assert_eq!(identity["model"], "Qwen3.8-27B");
        assert!(identity["checked_at"].is_string());
    }

    #[tokio::test]
    async fn identity_report_skips_runtimes_without_a_configured_command() {
        let identities = HashMap::from([("no-identity".to_string(), vec![])]);
        let report = identity_report(&identities).await;
        assert!(report.as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn identity_command_reports_sanitized_fields_and_drops_the_rest() {
        let command = vec![
            "sh".into(),
            "-c".into(),
            r#"printf '%s' '{"application":{"name":"ToshLLM","version":"1.2.3"},"build":"2024-01-01","commit":"abcdef0123","model":"Qwen2.5-7B","pid":12345,"secret":"drop-me"}'"#
                .into(),
        ];
        let value = run_identity(&command, Duration::from_secs(5)).await;
        assert_eq!(value["application"]["name"], "ToshLLM");
        assert_eq!(value["application"]["version"], "1.2.3");
        assert_eq!(value["build"], "2024-01-01");
        assert_eq!(value["commit"], "abcdef0123");
        assert_eq!(value["model"], "Qwen2.5-7B");
        assert!(value["checked_at"].is_string());
        assert!(value.get("pid").is_none());
        assert!(value.get("secret").is_none());
    }

    #[test]
    fn identity_sanitisation_drops_extra_fields_and_rejects_overlong_or_non_hex_values() {
        let long_name = "x".repeat(81);
        let raw = json!({
            "application": {"name": long_name, "version": "ok"},
            "commit": "not-hex",
            "build": "fine",
            "unexpected": "drop-me",
        });
        let sanitized = sanitize_identity(raw.as_object().unwrap());
        assert!(sanitized["application"].get("name").is_none());
        assert_eq!(sanitized["application"]["version"], "ok");
        assert!(sanitized.get("commit").is_none());
        assert_eq!(sanitized["build"], "fine");
        assert!(sanitized.get("unexpected").is_none());
    }

    #[test]
    fn identity_keeps_a_bounded_clean_model_list() {
        let mut models: Vec<Value> = vec![
            json!({"id": "qwen3.8-27b-mtp", "label": "Qwen3.8-27B IQ3_S", "context_tokens": 131072, "path": "/drop/me"}),
            json!({"id": "no-label", "label": "bad\nlabel", "context_tokens": 0}),
            json!({"id": "bad id!", "label": "dropped"}),
            json!({"label": "no id"}),
        ];
        models.extend((0..40).map(|index| json!({"id": format!("extra-{index}")})));
        let raw = json!({"servable": ["qwen3.8-27b-mtp"], "models": models});
        let sanitized = sanitize_identity(raw.as_object().unwrap());
        let kept = sanitized["models"].as_array().unwrap();
        assert_eq!(kept.len(), 32);
        assert_eq!(
            kept[0],
            json!({"id": "qwen3.8-27b-mtp", "label": "Qwen3.8-27B IQ3_S", "context_tokens": 131072})
        );
        assert_eq!(kept[1], json!({"id": "no-label"}));
        assert_eq!(kept[2]["id"], "extra-0");
        // An older runtime reports `servable` only, and that stays valid.
        let older = sanitize_identity(json!({"servable": ["a"]}).as_object().unwrap());
        assert!(older.get("models").is_none());
    }

    #[tokio::test]
    async fn identity_command_timeout_reports_an_error_without_hanging() {
        let command = vec!["sh".into(), "-c".into(), "sleep 5".into()];
        let value = run_identity(&command, Duration::from_millis(200)).await;
        assert_eq!(value["error"], "timeout");
    }

    #[tokio::test]
    async fn identity_command_failure_reports_an_error() {
        let command = vec!["sh".into(), "-c".into(), "exit 1".into()];
        let value = run_identity(&command, Duration::from_secs(5)).await;
        assert_eq!(value["error"], "exec_failed");
    }

    #[tokio::test]
    async fn identity_command_invalid_json_reports_an_error() {
        let command = vec!["sh".into(), "-c".into(), "printf 'not json'".into()];
        let value = run_identity(&command, Duration::from_secs(5)).await;
        assert_eq!(value["error"], "invalid_output");
    }
}
