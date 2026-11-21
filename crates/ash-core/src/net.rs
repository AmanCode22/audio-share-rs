//! LAN helpers: enumerate local IPv4 addresses and pick the best default,
//! mirroring the original project's behaviour.

use std::net::IpAddr;

/// All non-loopback IPv4 addresses of this machine.
pub fn lan_addresses() -> Vec<IpAddr> {
    let mut v = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() {
                continue;
            }
            if let IpAddr::V4(ip) = iface.ip() {
                if !v.contains(&IpAddr::V4(ip)) {
                    v.push(IpAddr::V4(ip));
                }
            }
        }
    }
    v
}

fn is_private_v4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    matches!(
        (o[0], o[1]),
        (10, _) | (172, 16..=31) | (192, 168) | (169, 254)
    )
}

/// Pick the most likely LAN-facing address (private ranges first), like the
/// original `select_default_address`.
pub fn pick_default_address(addrs: &[IpAddr]) -> Option<IpAddr> {
    for a in addrs {
        if let IpAddr::V4(ip) = a {
            if is_private_v4(*ip) {
                return Some(*a);
            }
        }
    }
    addrs.first().copied()
}

/// Convenience: default LAN address.
pub fn default_lan_address() -> Option<IpAddr> {
    let addrs = lan_addresses();
    pick_default_address(&addrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn private_selection() {
        let lan = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        let pub_ = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        assert_eq!(pick_default_address(&[pub_, lan]), Some(lan));
        assert_eq!(pick_default_address(&[pub_]), Some(pub_));
        assert_eq!(pick_default_address(&[]), None);
    }
}
