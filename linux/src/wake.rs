//! Waking the Mac from Linux (Wake-on-LAN): its hardware address is learned while it's connected
//! and kept in ~/.config/sweeflow/mac-address. Best effort: a MacBook only wakes for network
//! access on power, and on Wi-Fi only with "Wake for network access" on (or a Bonjour sleep
//! proxy, like an Apple TV, on the network).

use std::{fs, net::IpAddr, process::Command};

fn file() -> std::path::PathBuf {
    crate::config::dir().join("mac-address")
}

/// Notes the Mac's hardware address (from the neighbour table, now that it's talking to us).
pub fn remember(ip: IpAddr) {
    let Ok(out) = Command::new("ip").args(["neigh", "show", &ip.to_string()]).output() else { return };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(mac) = text.split_whitespace().skip_while(|w| *w != "lladdr").nth(1) else { return };
    if parse(mac).is_some() && fs::read_to_string(file()).ok().as_deref() != Some(mac) {
        let _ = fs::write(file(), mac);
        log::info!("Mac's hardware address {mac} noted (for waking it)");
    }
}

fn parse(mac: &str) -> Option<[u8; 6]> {
    let bytes: Vec<u8> = mac.split(':').filter_map(|b| u8::from_str_radix(b, 16).ok()).collect();
    bytes.try_into().ok()
}

/// The magic packet: six 0xFF bytes, then the address sixteen times.
fn packet(mac: [u8; 6]) -> Vec<u8> {
    let mut p = vec![0xff; 6];
    for _ in 0..16 {
        p.extend_from_slice(&mac);
    }
    p
}

pub fn wake() {
    let Some(mac) = fs::read_to_string(file()).ok().and_then(|m| parse(m.trim())) else {
        return log::info!("can't wake the Mac: its address isn't known yet");
    };
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else { return };
    let _ = sock.set_broadcast(true);
    for _ in 0..3 {
        for port in [7, 9] {
            let _ = sock.send_to(&packet(mac), ("255.255.255.255", port));
        }
    }
    log::info!("sent a wake-up to the Mac");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_is_ff_then_sixteen_addresses() {
        let mac = parse("a1:b2:c3:d4:e5:f6").unwrap();
        let p = packet(mac);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xff; 6]);
        assert_eq!(&p[96..], &[0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6]);
    }

    #[test]
    fn bad_addresses_are_rejected() {
        assert!(parse("a1:b2:c3").is_none());
        assert!(parse("zz:b2:c3:d4:e5:f6").is_none());
    }
}
