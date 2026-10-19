//! Outbound access for jobs that asked for it (M5). Every attempt network is
//! internal; for a job with outbound access the only way out is this proxy in
//! the supervisor. It answers only containers on a registered outbound
//! network, resolves each destination itself and connects to the address it
//! checked, and only to a public one: a job reaches package registries and the
//! internet, but not the operator's LAN, the host or Poundlock's own services.
//!
//! A Git fetch network (L13) is narrower and also wider: its container may
//! reach only the one repository host the controller approved for the job, on
//! port 443, at whatever address that host has, so a self-hosted Git server on
//! the operator's network still works.
use anyhow::{Context, Result, bail};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub(super) const PORT: u16 = 7333;
/// Registries and most of the web; other ports stay closed.
const PORTS: [u16; 2] = [80, 443];
const HEADER_LIMIT: usize = 16 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Open tunnels at once, in all and per container, so one job cannot use up
/// the supervisor's descriptors (review of M5).
const MAX_CONNECTIONS: usize = 256;
const MAX_PER_PEER: usize = 64;
/// A tunnel lasts at most as long as the default job wall time.
const TUNNEL_LIFETIME: Duration = Duration::from_secs(3600);

/// An outbound network: name, subnet base and prefix, and the one host it is
/// limited to when it is a Git fetch network.
type Network = (String, IpAddr, u8, Option<String>);

/// The outbound attempt networks.
#[derive(Clone, Default)]
pub(super) struct Egress {
    networks: Arc<Mutex<Vec<Network>>>,
}

impl Egress {
    pub fn allow(&self, name: &str, subnet: &str) -> Result<()> {
        self.register(name, subnet, None)
    }

    /// A network whose containers may reach only `host`, on port 443.
    pub fn allow_only(&self, name: &str, subnet: &str, host: &str) -> Result<()> {
        self.register(name, subnet, Some(host.to_ascii_lowercase()))
    }

    fn register(&self, name: &str, subnet: &str, only: Option<String>) -> Result<()> {
        let (base, prefix) = subnet
            .split_once('/')
            .context("network subnet has no prefix")?;
        let base: IpAddr = base.parse().context("network subnet address")?;
        let prefix: u8 = prefix.parse().context("network subnet prefix")?;
        let mut networks = self.networks.lock().unwrap_or_else(|p| p.into_inner());
        networks.retain(|(known, ..)| known != name);
        networks.push((name.to_owned(), base, prefix, only));
        Ok(())
    }

    pub fn forget(&self, name: &str) {
        let mut networks = self.networks.lock().unwrap_or_else(|p| p.into_inner());
        networks.retain(|(known, ..)| known != name);
    }

    pub fn outbound(&self, name: &str) -> bool {
        let networks = self.networks.lock().unwrap_or_else(|p| p.into_inner());
        networks.iter().any(|(known, ..)| known == name)
    }

    /// `None` when the peer is on no outbound network; otherwise the host it
    /// is limited to, if any.
    fn admits(&self, peer: IpAddr) -> Option<Option<String>> {
        let networks = self.networks.lock().unwrap_or_else(|p| p.into_inner());
        networks
            .iter()
            .find(|(_, base, prefix, _)| in_subnet(peer, *base, *prefix))
            .map(|(.., only)| only.clone())
    }
}

/// The environment a container on an outbound network gets. Model traffic to
/// the supervisor stays direct.
pub(super) fn proxy_environment() -> Vec<String> {
    let proxy = format!("http://poundlock-supervisor:{PORT}");
    let direct = "poundlock-supervisor,localhost,127.0.0.1";
    vec![
        format!("HTTP_PROXY={proxy}"),
        format!("HTTPS_PROXY={proxy}"),
        format!("http_proxy={proxy}"),
        format!("https_proxy={proxy}"),
        format!("NO_PROXY={direct}"),
        format!("no_proxy={direct}"),
        "NODE_USE_ENV_PROXY=1".to_owned(),
    ]
}

pub(super) async fn start(egress: Egress) -> Result<tokio::task::JoinHandle<()>> {
    let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
    Ok(tokio::spawn(serve(listener, egress, reachable)))
}

/// What a job may reach: a public address on port 80 or 443.
pub(super) fn reachable(address: SocketAddr) -> bool {
    PORTS.contains(&address.port()) && is_public(address.ip())
}

pub(super) async fn serve(
    listener: TcpListener,
    egress: Egress,
    permitted: fn(SocketAddr) -> bool,
) {
    let total = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let per_peer: Arc<Mutex<std::collections::HashMap<IpAddr, usize>>> = Arc::default();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // Out of descriptors, for instance: wait rather than spin.
                tracing::warn!(%error, "job egress accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = total.clone().try_acquire_owned() else {
            continue;
        };
        {
            let mut counts = per_peer.lock().unwrap_or_else(|p| p.into_inner());
            let open = counts.entry(peer.ip()).or_default();
            if *open >= MAX_PER_PEER {
                continue;
            }
            *open += 1;
        }
        let egress = egress.clone();
        let per_peer = per_peer.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle(stream, peer, &egress, permitted).await {
                tracing::debug!(%error, "job egress connection ended");
            }
            let mut counts = per_peer.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(open) = counts.get_mut(&peer.ip()) {
                *open = open.saturating_sub(1);
                if *open == 0 {
                    counts.remove(&peer.ip());
                }
            }
        });
    }
}

async fn handle(
    mut client: TcpStream,
    peer: SocketAddr,
    egress: &Egress,
    permitted: fn(SocketAddr) -> bool,
) -> Result<()> {
    let Some(only) = egress.admits(peer.ip()) else {
        return refuse(
            &mut client,
            "403 Forbidden",
            "This container has no outbound access.",
        )
        .await;
    };
    let (head, rest) = tokio::time::timeout(HEADER_TIMEOUT, read_head(&mut client))
        .await
        .context("request headers timed out")??;
    let request = match parse(&head) {
        Ok(request) => request,
        Err(error) => return refuse(&mut client, "400 Bad Request", &error.to_string()).await,
    };
    let resolved = match &only {
        Some(host) => fetch_destination(host, &request).await,
        None => destination(&request.host, request.port, permitted).await,
    };
    let address = match resolved {
        Ok(address) => address,
        Err(error) => return refuse(&mut client, "403 Forbidden", &error.to_string()).await,
    };
    let mut upstream =
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => stream,
            _ => {
                return refuse(
                    &mut client,
                    "502 Bad Gateway",
                    "The destination did not answer.",
                )
                .await;
            }
        };
    match request.forward {
        None => {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?
        }
        Some(head) => upstream.write_all(head.as_bytes()).await?,
    }
    upstream.write_all(&rest).await?;
    let _ = tokio::time::timeout(
        TUNNEL_LIFETIME,
        tokio::io::copy_bidirectional(&mut client, &mut upstream),
    )
    .await;
    Ok(())
}

async fn refuse(client: &mut TcpStream, status: &str, reason: &str) -> Result<()> {
    let body = format!("Poundlock job egress: {reason}\n");
    client
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    Ok(())
}

/// The request head, and any bytes the client sent after it.
async fn read_head(client: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((
                String::from_utf8(buffer).context("request head is not UTF-8")?,
                rest,
            ));
        }
        if buffer.len() > HEADER_LIMIT {
            bail!("request head is too large");
        }
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed before the request head ended");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

#[derive(Debug, PartialEq)]
struct Request {
    host: String,
    port: u16,
    /// For plain HTTP, the head to send upstream: origin-form, without proxy
    /// headers, and closed after one response. `None` for a CONNECT tunnel.
    forward: Option<String>,
}

fn parse(head: &str) -> Result<Request> {
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(version), None) => (method, target, version),
        _ => bail!("malformed request line"),
    };
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(target, None)?;
        return Ok(Request {
            host,
            port,
            forward: None,
        });
    }
    let rest = target
        .strip_prefix("http://")
        .context("use CONNECT for HTTPS, or an absolute http:// URL")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = split_authority(authority, Some(80))?;
    let mut forward = format!("{method} {path} {version}\r\n");
    for header in lines.filter(|line| !line.is_empty()) {
        let name = header
            .split(':')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "proxy-connection" | "proxy-authorization" | "connection" | "keep-alive"
        ) {
            forward.push_str(header);
            forward.push_str("\r\n");
        }
    }
    forward.push_str("Connection: close\r\n\r\n");
    Ok(Request {
        host,
        port,
        forward: Some(forward),
    })
}

fn split_authority(authority: &str, default_port: Option<u16>) -> Result<(String, u16)> {
    if authority.contains('@') {
        bail!("credentials in the target are not accepted");
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .context("unterminated IPv6 address")?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match (port, default_port) {
        (Some(port), _) => port.parse().context("invalid port")?,
        (None, Some(port)) => port,
        (None, None) => bail!("CONNECT needs a port"),
    };
    if host.is_empty() {
        bail!("no destination host");
    }
    Ok((host.to_owned(), port))
}

/// The first resolved address, if every address the name resolves to is one a
/// job may reach: a name that also points inside is refused, not filtered.
async fn destination(
    host: &str,
    port: u16,
    permitted: fn(SocketAddr) -> bool,
) -> Result<SocketAddr> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("{host} does not resolve"))?
        .collect();
    let first = *addresses
        .first()
        .with_context(|| format!("{host} does not resolve"))?;
    if let Some(refused) = addresses.iter().find(|address| !permitted(**address)) {
        // The resolved address stays in the log: a job must not map internal names.
        tracing::debug!(%host, %refused, "job egress refused a destination");
        bail!("{host} is not a public address on port 80 or 443");
    }
    Ok(first)
}

/// A Git fetch network's one destination: a CONNECT tunnel to the approved
/// host on 443. The operator approved the host by name, so any address it
/// resolves to is allowed, the operator's own network included.
async fn fetch_destination(host: &str, request: &Request) -> Result<SocketAddr> {
    if request.forward.is_some() || request.port != 443 || !request.host.eq_ignore_ascii_case(host)
    {
        bail!("a Git fetch reaches only its repository host on port 443");
    }
    tokio::net::lookup_host((host, 443))
        .await
        .with_context(|| format!("{host} does not resolve"))?
        .next()
        .with_context(|| format!("{host} does not resolve"))
}

fn in_subnet(address: IpAddr, base: IpAddr, prefix: u8) -> bool {
    match (address, base) {
        (IpAddr::V4(address), IpAddr::V4(base)) if prefix <= 32 => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(address) & mask == u32::from(base) & mask
        }
        (IpAddr::V6(address), IpAddr::V6(base)) if prefix <= 128 => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(address) & mask == u128::from(base) & mask
        }
        _ => false,
    }
}

/// Whether an address is on the public internet: not private, loopback,
/// link-local, shared (CGNAT), multicast, reserved or documentation space.
pub(super) fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_v4(address),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(mapped) => public_v4(mapped),
            None => public_v6(address),
        },
    }
}

fn public_v4(address: Ipv4Addr) -> bool {
    let refused: [(Ipv4Addr, u8); 15] = [
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 88, 99, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(224, 0, 0, 0), 4),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
    ];
    !refused
        .iter()
        .any(|(base, prefix)| in_subnet(IpAddr::V4(address), IpAddr::V4(*base), *prefix))
}

fn public_v6(address: Ipv6Addr) -> bool {
    let refused: [(Ipv6Addr, u8); 13] = [
        (Ipv6Addr::UNSPECIFIED, 128),
        (Ipv6Addr::LOCALHOST, 128),
        // IPv4-compatible, NAT64 (well-known and local), 6to4 and Teredo carry an
        // IPv4 address, which may be a private one.
        (Ipv6Addr::UNSPECIFIED, 96),
        (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
        (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
        (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
        (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
        (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
        (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    ];
    !refused
        .iter()
        .any(|(base, prefix)| in_subnet(IpAddr::V6(address), IpAddr::V6(*base), *prefix))
}

#[cfg(test)]
#[path = "runner_egress_tests.rs"]
mod tests;
