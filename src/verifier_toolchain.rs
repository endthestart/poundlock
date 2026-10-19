use serde_json::{Value, json};

/// CPython series installed on the worker/verifier image (not Debian's python3).
pub const PYTHON_SERIES: &str = "3.14";
/// uv release copied into `/usr/local/bin/uv` at image build.
pub const UV_VERSION: &str = "0.12.24";
pub const PI_VERSION: &str = "1.1.0";

pub const WRITABLE_EXEC_PATHS: [&str; 3] = ["/tmp", "/work", "/home/worker"];

/// Worker PATH: image uv and python3.14 live under `/usr/local/bin`.
pub const WORKER_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Docker tmpfs defaults include `noexec` unless `exec` is explicit.
pub fn work_volume_tmpfs_opts() -> &'static str {
    "size=2g,uid=10001,gid=10001,mode=0700,nosuid,nodev,exec"
}

/// The most memory one attempt holds at once: its three RAM-backed volumes
/// (work, trusted, verify) and one container's memory limit with its /tmp and
/// home tmpfs (L15). Keep in step with the sizes in this file and the 4 GiB
/// container limit in `runner_docker`.
pub const ATTEMPT_MEMORY_BYTES: u64 = (3 * 2048 + 4096 + 256 + 512) * 1024 * 1024;

/// How many attempts may run at once: what was asked for, but no more than
/// fit in half of the host's memory, and at least one.
pub fn capacity_for_memory(requested: u16, host_memory_bytes: Option<u64>) -> u16 {
    let Some(total) = host_memory_bytes else {
        return requested;
    };
    let fits = (total / 2 / ATTEMPT_MEMORY_BYTES).max(1);
    requested.min(u16::try_from(fits).unwrap_or(u16::MAX))
}

pub fn tmp_tmpfs_opts() -> &'static str {
    "rw,exec,nosuid,nodev,size=256m,uid=10001,gid=10001,mode=0700"
}

/// Package managers keep their caches under home (`~/.npm`, `~/.cache`); 32 MiB
/// made `npm ci` fail with ENOSPC on ordinary projects.
pub fn home_tmpfs_opts() -> &'static str {
    "rw,exec,nosuid,nodev,size=512m,uid=10001,gid=10001,mode=0700"
}

pub fn advertised() -> Value {
    json!({
        "python": PYTHON_SERIES,
        "uv": UV_VERSION,
        "writable_exec_paths": WRITABLE_EXEC_PATHS,
    })
}

pub fn provenance_toolchain(observed_python: Option<&str>, observed_uv: Option<&str>) -> Value {
    json!({
        "python": PYTHON_SERIES,
        "uv": UV_VERSION,
        "writable_exec": true,
        "writable_exec_paths": WRITABLE_EXEC_PATHS,
        "observed_python": observed_python,
        "observed_uv": observed_uv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_bounded_by_half_the_host_memory() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(capacity_for_memory(4, None), 4);
        assert_eq!(capacity_for_memory(4, Some(16 * GIB)), 1);
        assert_eq!(capacity_for_memory(4, Some(64 * GIB)), 2);
        assert_eq!(capacity_for_memory(2, Some(512 * GIB)), 2);
        assert_eq!(capacity_for_memory(1, Some(GIB)), 1);
    }

    #[test]
    fn advertised_names_python_uv_and_writable_exec_paths() {
        let value = advertised();
        assert_eq!(value["python"], PYTHON_SERIES);
        assert_eq!(value["uv"], UV_VERSION);
        assert_eq!(
            value["writable_exec_paths"],
            json!(["/tmp", "/work", "/home/worker"])
        );
    }

    #[test]
    fn writable_mounts_request_exec_and_never_noexec() {
        for opts in [
            work_volume_tmpfs_opts(),
            tmp_tmpfs_opts(),
            home_tmpfs_opts(),
        ] {
            assert!(
                opts.split(',').any(|part| part == "exec"),
                "missing exec: {opts}"
            );
            assert!(
                !opts.split(',').any(|part| part == "noexec"),
                "must not set noexec: {opts}"
            );
        }
    }
}
