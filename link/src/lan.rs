//! Which peers count as "on the LAN": only they may pair (4.4).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Private (RFC 1918), link-local, unique local (fc00::/7) or loopback.
/// Carrier-grade NAT space and everything public are not.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_lan(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4_is_lan(v4),
            None => v6_is_lan(v6),
        },
    }
}

fn v4_is_lan(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local() || ip.is_loopback()
}

fn v6_is_lan(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback() || first & 0xffc0 == 0xfe80 || first & 0xfe00 == 0xfc00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_private_link_local_and_unique_local_addresses_are_lan() {
        for lan in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.20",
            "169.254.3.4",
            "127.0.0.1",
            "::1",
            "fe80::1",
            "fd12:3456::1",
            "fc00::1",
            "::ffff:192.168.1.2",
        ] {
            assert!(is_lan(lan.parse().unwrap()), "{lan}");
        }
        for public in [
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.1",
            "2001:db8::1",
            "2408:8000::1",
            "::ffff:8.8.8.8",
            "fec0::1",
        ] {
            assert!(!is_lan(public.parse().unwrap()), "{public}");
        }
    }
}
