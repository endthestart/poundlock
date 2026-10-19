use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum NetworkPolicy {
    #[default]
    System,
    Interface {
        name: String,
    },
}

impl NetworkPolicy {
    pub(super) fn validate(&self) -> Result<()> {
        if let Self::Interface { name } = self {
            require_connected(name)?;
        }
        Ok(())
    }

    pub(super) fn client(&self, builder: reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder> {
        match self {
            Self::System => Ok(builder),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Interface { name } => Ok(builder.interface(name)),
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            Self::Interface { .. } => bail!("Interface binding is supported on Linux and macOS"),
        }
    }

    pub(super) fn report(&self) -> serde_json::Value {
        match self {
            Self::System => match interfaces() {
                Ok(interfaces) => serde_json::json!({"mode":"system", "interfaces":interfaces}),
                Err(error) => serde_json::json!({"mode":"system", "message":error.to_string()}),
            },
            Self::Interface { name } => {
                match interfaces().and_then(|items| {
                    selected(name, items.clone()).map(|selected| (selected, items))
                }) {
                    Ok((interface, interfaces)) => serde_json::json!({
                        "mode":"interface",
                        "status": if interface.connected { "connected" } else { "disconnected" },
                        "interface": interface.name, "addresses": interface.addresses, "mac": interface.mac,
                        "interfaces":interfaces
                    }),
                    Err(error) => {
                        serde_json::json!({"mode":"interface", "status":"unavailable", "interface":name, "message":error.to_string()})
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Interface {
    pub name: String,
    pub connected: bool,
    pub addresses: Vec<IpAddr>,
    pub mac: Option<String>,
    pub broadcasts: Vec<Ipv4Addr>,
    pub kind: Option<String>,
    pub driver: Option<String>,
    pub mtu: Option<u64>,
}

/// Inspect local interfaces without changing routes or connecting to any host.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn interfaces() -> Result<Vec<Interface>> {
    let mut head = std::ptr::null_mut();
    // SAFETY: getifaddrs initializes head; its linked list remains valid until
    // freeifaddrs below. Every sockaddr cast is guarded by its address family.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut found = BTreeMap::<String, Interface>::new();
        let mut cursor = head;
        while let Some(item) = cursor.as_ref() {
            cursor = item.ifa_next;
            if item.ifa_name.is_null() || item.ifa_flags & libc::IFF_LOOPBACK as u32 != 0 {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(item.ifa_name)
                .to_string_lossy()
                .into_owned();
            let entry = found.entry(name.clone()).or_insert_with(|| Interface {
                name,
                ..Default::default()
            });
            let up = libc::IFF_UP as u32 | libc::IFF_RUNNING as u32;
            entry.connected |= item.ifa_flags & up == up;
            let Some(address) = item.ifa_addr.as_ref() else {
                continue;
            };
            match i32::from(address.sa_family) {
                libc::AF_INET => {
                    let addr = &*item.ifa_addr.cast::<libc::sockaddr_in>();
                    let ip = Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes());
                    entry.addresses.push(ip.into());
                    if item.ifa_flags & libc::IFF_BROADCAST as u32 != 0
                        && !item.ifa_netmask.is_null()
                    {
                        let mask = &*item.ifa_netmask.cast::<libc::sockaddr_in>();
                        if let Some(broadcast) =
                            broadcast(ip, Ipv4Addr::from(mask.sin_addr.s_addr.to_ne_bytes()))
                        {
                            entry.broadcasts.push(broadcast);
                        }
                    }
                }
                libc::AF_INET6 => {
                    let addr = &*item.ifa_addr.cast::<libc::sockaddr_in6>();
                    entry
                        .addresses
                        .push(Ipv6Addr::from(addr.sin6_addr.s6_addr).into());
                }
                #[cfg(target_os = "linux")]
                libc::AF_PACKET => {
                    let addr = &*item.ifa_addr.cast::<libc::sockaddr_ll>();
                    if addr.sll_halen == 6 {
                        entry.mac = format_mac(&addr.sll_addr[..6]);
                    }
                }
                #[cfg(target_os = "macos")]
                libc::AF_LINK => {
                    let addr = &*item.ifa_addr.cast::<libc::sockaddr_dl>();
                    let start = usize::from(addr.sdl_nlen);
                    if addr.sdl_alen == 6 && start + 6 <= addr.sdl_data.len() {
                        let bytes: Vec<u8> = addr.sdl_data[start..start + 6]
                            .iter()
                            .map(|byte| *byte as u8)
                            .collect();
                        entry.mac = format_mac(&bytes);
                    }
                }
                _ => {}
            }
        }
        libc::freeifaddrs(head);
        for entry in found.values_mut() {
            entry.addresses.sort();
            entry.addresses.dedup();
            entry.connected &= entry.addresses.iter().any(usable_address);
            entry.broadcasts.sort();
            entry.broadcasts.dedup();
            #[cfg(target_os = "linux")]
            {
                let path = std::path::Path::new("/sys/class/net").join(&entry.name);
                entry.kind = Some(
                    if path.join("wireless").exists() {
                        "wireless"
                    } else if path.join("device").exists() {
                        "physical"
                    } else {
                        "virtual"
                    }
                    .into(),
                );
                entry.driver = std::fs::read_link(path.join("device/driver"))
                    .ok()
                    .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
                entry.mtu = std::fs::read_to_string(path.join("mtu"))
                    .ok()
                    .and_then(|s| s.trim().parse().ok());
            }
        }
        Ok(found.into_values().collect())
    }
}

fn broadcast(ip: Ipv4Addr, mask: Ipv4Addr) -> Option<Ipv4Addr> {
    let mask = u32::from(mask);
    let host = !mask;
    // Point-to-point and noncontiguous masks do not supply a broadcast target.
    (host > 1 && host < u32::MAX && host & host.wrapping_add(1) == 0)
        .then(|| Ipv4Addr::from(u32::from(ip) | host))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn interfaces() -> Result<Vec<Interface>> {
    bail!("network interface selection is supported on Linux and macOS")
}

fn usable_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_unspecified() && !address.is_loopback() && !address.is_multicast()
        }
        IpAddr::V6(address) => {
            !address.is_unspecified() && !address.is_loopback() && !address.is_multicast()
        }
    }
}

fn format_mac(bytes: &[u8]) -> Option<String> {
    (bytes.iter().any(|byte| *byte != 0)).then(|| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    })
}

pub(super) fn selected(name: &str, interfaces: Vec<Interface>) -> Result<Interface> {
    if name.is_empty() {
        bail!(
            "Interface mode requires a network.name in node.toml. Run poundlock node-interfaces to list this machine's connections, or choose system mode."
        );
    }
    interfaces.into_iter().find(|interface| interface.name == name)
        .ok_or_else(|| anyhow::anyhow!("Selected network interface '{name}' was not found. Run poundlock node-interfaces and update node.toml; no other interface was selected."))
}

pub(super) fn require_connected(name: &str) -> Result<()> {
    let interface = selected(name, interfaces()?)?;
    if !interface.connected {
        bail!(
            "Selected network interface '{name}' is disconnected or has no usable IP address. Connect it before starting inference; no other interface was selected."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_uses_each_interfaces_netmask() {
        assert_eq!(
            broadcast(
                "192.0.2.9".parse().unwrap(),
                "255.255.255.0".parse().unwrap()
            ),
            Some("192.0.2.255".parse().unwrap())
        );
        assert_eq!(
            broadcast(
                "192.0.2.9".parse().unwrap(),
                "255.255.255.252".parse().unwrap()
            ),
            Some("192.0.2.11".parse().unwrap())
        );
        for mask in [
            "255.255.255.255",
            "255.255.255.254",
            "255.0.255.0",
            "0.0.0.0",
        ] {
            assert_eq!(
                broadcast("192.0.2.9".parse().unwrap(), mask.parse().unwrap()),
                None
            );
        }
    }

    #[tokio::test]
    async fn interface_binding_never_retries_with_system_routing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/", axum::routing::get(|| async { "ok" })),
            )
            .await
            .unwrap();
        });
        let builder = || {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(1))
        };
        let url = format!("http://{address}/");
        let system = NetworkPolicy::System
            .client(builder())
            .unwrap()
            .build()
            .unwrap();
        assert!(system.get(&url).send().await.unwrap().status().is_success());
        let loopback = if cfg!(target_os = "macos") {
            "lo0"
        } else {
            "lo"
        };
        let valid = NetworkPolicy::Interface {
            name: loopback.into(),
        }
        .client(builder())
        .unwrap()
        .build()
        .unwrap();
        assert!(
            valid.get(&url).send().await.unwrap().status().is_success(),
            "an existing interface must support bound requests"
        );
        let bound = NetworkPolicy::Interface {
            name: "pl-missing-nic".into(),
        }
        .client(builder())
        .unwrap()
        .build();
        if let Ok(bound) = bound {
            assert!(
                bound.get(&url).send().await.is_err(),
                "bound traffic must not fall back to the working system route"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn explicit_interface_mode_fails_before_controller_contact_when_name_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let config_file = directory.path().join("node.toml");
        std::fs::write(
            &config_file,
            "network = { mode = 'interface', name = '' }\n[runtimes.ready]",
        )
        .unwrap();
        let credential_file = directory.path().join("credential.json");
        let error = crate::node::run(crate::node::NodeOptions {
            controller: "http://127.0.0.1:9".into(),
            name: "test".into(),
            enrollment_token: Some("test".into()),
            credential_file: credential_file.clone(),
            config_file,
            data_dir: directory.path().to_owned(),
        })
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("Interface mode requires"),
            "{error}"
        );
        assert!(!credential_file.exists());
    }

    #[test]
    fn selected_disconnected_wired_interface_does_not_fall_back_to_wifi() {
        let wifi = Interface {
            name: "wifi".into(),
            connected: true,
            ..Default::default()
        };
        let wired = Interface {
            name: "wired".into(),
            connected: false,
            ..Default::default()
        };
        assert!(
            !selected("wired", vec![wifi.clone(), wired])
                .unwrap()
                .connected
        );
        assert!(selected("missing", vec![wifi.clone()]).is_err());
        assert!(selected("", vec![wifi]).is_err());
    }

    #[test]
    fn system_routing_does_not_require_one_interface() {
        let config: crate::node::NodeConfig =
            toml::from_str(crate::node::example_config()).unwrap();
        assert!(matches!(config.network, NetworkPolicy::System));
        config.network.validate().unwrap();
        let legacy: crate::node::NodeConfig = toml::from_str("heartbeat_seconds = 3").unwrap();
        assert!(matches!(legacy.network, NetworkPolicy::System));
    }

    #[test]
    fn local_network_addresses_are_valid_without_assuming_internet_access() {
        for address in ["127.0.0.1", "::1", "0.0.0.0", "224.0.0.1"] {
            assert!(!usable_address(&address.parse().unwrap()));
        }
        assert!(usable_address(&"192.168.1.10".parse().unwrap()));
        assert!(usable_address(&"fd00::10".parse().unwrap()));
        assert!(usable_address(&"169.254.1.2".parse().unwrap()));
        assert!(usable_address(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn native_interface_inventory_has_unique_names() {
        let items = interfaces().unwrap();
        let names: std::collections::BTreeSet<_> = items.iter().map(|item| &item.name).collect();
        assert_eq!(items.len(), names.len());
    }
}
