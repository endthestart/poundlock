#[path = "wake_relay.rs"]
mod relay;
pub use relay::{request_magic_packet, run_relay};

use std::net::UdpSocket;

pub fn parse_mac(value: &str) -> Result<[u8; 6], String> {
    let parts = value.split([':', '-']).collect::<Vec<_>>();
    if parts.len() != 6 {
        return Err("MAC address must contain six octets".into());
    }
    let mut mac = [0_u8; 6];
    for (index, part) in parts.into_iter().enumerate() {
        mac[index] = u8::from_str_radix(part, 16)
            .map_err(|_| "MAC address contains an invalid octet".to_string())?;
    }
    Ok(mac)
}

pub fn send_magic_packet(mac: &str, broadcast: &str) -> Result<(), String> {
    let mac = parse_mac(mac)?;
    let mut packet = [0_u8; 102];
    packet[..6].fill(0xff);
    for index in 0..16 {
        packet[6 + index * 6..12 + index * 6].copy_from_slice(&mac);
    }
    let destination = if broadcast.contains(':') {
        broadcast.to_owned()
    } else {
        format!("{broadcast}:9")
    };
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|error| error.to_string())?;
    socket
        .set_broadcast(true)
        .map_err(|error| error.to_string())?;
    for _ in 0..3 {
        socket
            .send_to(&packet, &destination)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_mac_shapes() {
        assert_eq!(
            parse_mac("00:11:22:aa:BB:ff").unwrap(),
            [0, 17, 34, 170, 187, 255]
        );
        assert!(parse_mac("00:11").is_err());
    }
}
