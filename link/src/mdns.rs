//! LAN discovery with mDNS/DNS-SD (`_runo._tcp`), answered by Runo itself
//! rather than avahi or mdnsd (4.8). The TXT record holds only the device
//! ID (`id`), the protocol version (`v`) and whether the pairing window is
//! open (`pair`); the instance name is the device's name.
//!
//! The responder shares port 5353 with any other one on the host (avahi on
//! Raspberry Pi OS). It follows interfaces and addresses coming and going,
//! and renames the instance (`name (2)`) on a clash; clients know a device
//! by its ID, never by its name.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::lan::is_lan;
use crate::proto::{SERVICE, VERSION};

pub struct Advertiser {
    daemon: ServiceDaemon,
    id: String,
    name: String,
    port: u16,
    fullname: String,
}

fn info(id: &str, name: &str, port: u16, pairing: bool) -> Result<ServiceInfo> {
    // A host name of its own, so it never clashes with the system's.
    let host = format!("runo-{}.local.", id.replace('-', "").get(..8).unwrap_or(id));
    let properties = [
        ("id", id.to_owned()),
        ("v", VERSION.to_string()),
        ("pair", if pairing { "1" } else { "0" }.to_owned()),
    ];
    let info = ServiceInfo::new(SERVICE, name, &host, "", port, &properties[..])
        .map_err(|err| anyhow!("mDNS service: {err}"))?
        .enable_addr_auto();
    Ok(info)
}

impl Advertiser {
    /// Answers for device `id`, called `name`, on `port`.
    pub fn start(id: &str, name: &str, port: u16, pairing: bool) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(|err| anyhow!("mDNS: {err}"))?;
        let info = info(id, name, port, pairing)?;
        let fullname = info.get_fullname().to_owned();
        daemon
            .register(info)
            .map_err(|err| anyhow!("mDNS: {err}"))
            .context("announcing the device")?;
        Ok(Self {
            daemon,
            id: id.to_owned(),
            name: name.to_owned(),
            port,
            fullname,
        })
    }

    /// Announces the pairing window opening or closing.
    pub fn set_pairing(&self, pairing: bool) -> Result<()> {
        self.daemon
            .register(info(&self.id, &self.name, self.port, pairing)?)
            .map_err(|err| anyhow!("mDNS: {err}"))
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Says goodbye, so clients drop the device at once.
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }
}

/// A device found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub pairing: bool,
    /// Where it answers: LAN addresses first, IPv4 before IPv6, so a
    /// connection lands where the device counts it as local (only there
    /// may a client pair).
    pub addresses: Vec<SocketAddr>,
}

fn socket_address(ip: &ScopedIp, port: u16) -> SocketAddr {
    match ip {
        ScopedIp::V6(v6) => {
            let scope = if v6.addr().segments()[0] & 0xffc0 == 0xfe80 {
                v6.scope_id().index
            } else {
                0
            };
            SocketAddr::V6(SocketAddrV6::new(*v6.addr(), port, 0, scope))
        }
        other => SocketAddr::new(other.to_ip_addr(), port),
    }
}

/// The instance part of `fullname`, unescaped as far as names go.
fn instance(fullname: &str) -> String {
    let name = fullname
        .strip_suffix(SERVICE)
        .and_then(|name| name.strip_suffix('.'))
        .unwrap_or(fullname);
    name.replace("\\.", ".").replace("\\\\", "\\")
}

/// The devices that answer within `wait`, by ID.
pub async fn browse(wait: Duration) -> Result<Vec<Found>> {
    let daemon = ServiceDaemon::new().map_err(|err| anyhow!("mDNS: {err}"))?;
    let events = daemon
        .browse(SERVICE)
        .map_err(|err| anyhow!("mDNS: {err}"))?;
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let event = match tokio::time::timeout_at(deadline, events.recv_async()).await {
            Ok(Ok(event)) => event,
            Ok(Err(_)) | Err(_) => break,
        };
        match event {
            ServiceEvent::ServiceResolved(service) => {
                let Some(id) = service.get_property_val_str("id") else {
                    continue;
                };
                let version = service
                    .get_property_val_str("v")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let pairing = service.get_property_val_str("pair") == Some("1");
                let addresses: Vec<SocketAddr> = service
                    .get_addresses()
                    .iter()
                    .map(|ip| socket_address(ip, service.get_port()))
                    .collect();
                let entry = found.entry(id.to_owned()).or_insert_with(|| Found {
                    id: id.to_owned(),
                    name: instance(service.get_fullname()),
                    version,
                    pairing,
                    addresses: vec![],
                });
                entry.version = version;
                entry.pairing = pairing;
                for address in addresses {
                    if !entry.addresses.contains(&address) {
                        entry.addresses.push(address);
                    }
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                let name = instance(&fullname);
                found.retain(|_, device| device.name != name);
            }
            _ => {}
        }
    }
    // A device resolves more than once as its addresses arrive, so order
    // them only once all are in.
    for device in found.values_mut() {
        device.addresses.sort_by_key(|&a| preference(a));
    }
    let _ = daemon.stop_browse(SERVICE);
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

/// Sort key for a device's addresses: LAN before public, IPv4 before IPv6.
fn preference(address: SocketAddr) -> (bool, bool, SocketAddr) {
    (!is_lan(address.ip()), address.is_ipv6(), address)
}

/// Whether `ip` is one an mDNS answer could carry for a LAN device.
pub fn usable(ip: IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_come_back_from_full_names() {
        assert_eq!(instance("pi._runo._tcp.local."), "pi");
        assert_eq!(instance("my\\.pi._runo._tcp.local."), "my.pi");
        assert_eq!(instance("pi (2)._runo._tcp.local."), "pi (2)");
    }

    #[test]
    fn lan_addresses_come_before_public_ones() {
        let mut addresses: Vec<SocketAddr> = [
            "[2408:8256:2680:1f32::7ff9]:7340",
            "[fe80::1%2]:7340",
            "192.168.101.69:7340",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        addresses.sort_by_key(|&a| preference(a));
        let order: Vec<String> = addresses.iter().map(|a| a.to_string()).collect();
        assert_eq!(
            order,
            [
                "192.168.101.69:7340",
                "[fe80::1%2]:7340",
                "[2408:8256:2680:1f32::7ff9]:7340"
            ]
        );
    }

    /// Needs multicast on the host; run by hand with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn a_device_is_found_with_its_id_and_pairing_state() {
        let advertiser = Advertiser::start("abcd-efgh-jkmn-pqrs", "test-pi", 7399, true).unwrap();
        let found = browse(Duration::from_secs(3)).await.unwrap();
        let device = found
            .iter()
            .find(|d| d.id == "abcd-efgh-jkmn-pqrs")
            .unwrap();
        assert_eq!(device.name, "test-pi");
        assert!(device.pairing);
        assert!(!device.addresses.is_empty());
        drop(advertiser);
    }
}
