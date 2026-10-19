use anyhow::{Context, Result, bail};
use std::path::Path;

pub async fn request_magic_packet(socket: Option<&Path>, mac: &str, broadcast: &str) -> Result<()> {
    let Some(socket) = socket else {
        return super::send_magic_packet(mac, broadcast).map_err(anyhow::Error::msg);
    };
    #[cfg(unix)]
    {
        let response = reqwest::Client::builder()
            .unix_socket(socket)
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .post("http://localhost/wake")
            .json(&serde_json::json!({"mac":mac,"broadcast":broadcast}))
            .send()
            .await
            .context("wake relay unavailable; check its service and socket")?;
        if !response.status().is_success() {
            bail!(
                "wake relay rejected the packet request ({})",
                response.status()
            )
        }
        Ok(())
    }
    #[cfg(not(unix))]
    bail!("a wake relay socket requires a Unix controller")
}

#[cfg(unix)]
pub async fn run_relay(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("wake socket requires an absolute private directory")?;
    anyhow::ensure!(path.is_absolute(), "wake socket path must be absolute");
    let metadata = std::fs::symlink_metadata(parent)
        .context("create the private wake socket directory first")?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "wake socket directory must be private (mode 0700)"
    );
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_socket(),
                "refusing to replace a non-socket path"
            );
            match tokio::net::UnixStream::connect(path).await {
                Ok(_) => bail!("wake relay is already listening"),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?
                }
                Err(error) => return Err(error).context("inspect existing wake socket"),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect wake socket path"),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let app = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ready" }))
        .route("/wake", axum::routing::post(wake))
        .layer(axum::extract::DefaultBodyLimit::max(1024));
    tracing::info!("wake relay listening on its private socket");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(unix)]
async fn wake(
    axum::Json(input): axum::Json<WakeRequest>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, &'static str)> {
    // Accept a numeric destination only: no DNS or HTTP forwarding. The relay
    // emits only a fixed-format 102-byte WOL packet; it executes no commands.
    let destination = if input.broadcast.contains(':') {
        input.broadcast.clone()
    } else {
        format!("{}:9", input.broadcast)
    };
    if destination.parse::<std::net::SocketAddrV4>().is_err()
        || super::parse_mac(&input.mac).is_err()
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "invalid IPv4 destination or MAC address",
        ));
    }
    super::send_magic_packet(&input.mac, &destination).map_err(|_| {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "could not send wake packet",
        )
    })?;
    Ok(axum::Json(serde_json::json!({"packet_sent":true})))
}

#[cfg(unix)]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WakeRequest {
    mac: String,
    broadcast: String,
}

#[cfg(not(unix))]
pub async fn run_relay(_path: &Path) -> Result<()> {
    bail!("wake relay requires a Unix host")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn private_socket_relay_delivers_exact_packets_and_never_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("wake.sock");
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap().to_string();
        assert!(
            request_magic_packet(Some(&path), "02:00:00:00:00:01", &address)
                .await
                .is_err()
        );
        let server_path = path.clone();
        let server = tokio::spawn(async move { run_relay(&server_path).await.unwrap() });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await
        }
        request_magic_packet(Some(&path), "02:00:00:00:00:01", &address)
            .await
            .unwrap();
        let mut bytes = [0; 1024];
        for _ in 0..3 {
            let (count, _) = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                target.recv_from(&mut bytes),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(count, 102);
            assert_eq!(&bytes[..6], &[255; 6]);
            for chunk in bytes[6..102].chunks(6) {
                assert_eq!(chunk, &[2, 0, 0, 0, 0, 1])
            }
        }
        assert!(run_relay(&path).await.is_err());
        assert!(
            request_magic_packet(Some(&path), "invalid", &address)
                .await
                .is_err()
        );
        server.abort();
        let _ = server.await;
        assert!(
            request_magic_packet(Some(&path), "02:00:00:00:00:01", &address)
                .await
                .is_err()
        );
        let restarted = tokio::spawn(async move { run_relay(&path).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!restarted.is_finished());
        restarted.abort();
    }

    #[tokio::test]
    async fn startup_preserves_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("wake.sock");
        std::fs::write(&path, "keep").unwrap();
        assert!(run_relay(&path).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
    }
}
