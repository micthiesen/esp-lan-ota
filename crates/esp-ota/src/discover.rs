//! mDNS lookups for commissionable (`_matterc._udp`) and operational
//! (`_matter._tcp`) Matter nodes.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

/// Matches a commissionable node by its full 12-bit discriminator, or by the
/// 4-bit short one from a manual pairing code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discriminator {
    Long(u16),
    Short(u8),
}

impl Discriminator {
    pub fn matches(self, long: u16) -> bool {
        match self {
            Discriminator::Long(value) => value == long,
            Discriminator::Short(value) => u16::from(value) == long >> 8,
        }
    }
}

/// The operational instance name: `<compressed fabric id>-<node id>`.
pub fn operational_instance(compressed_fabric_id: u64, node_id: u64) -> String {
    format!("{compressed_fabric_id:016X}-{node_id:016X}")
}

/// A device in commissioning mode on the LAN.
pub fn find_commissionable(discriminator: Discriminator, timeout: Duration) -> Result<Option<SocketAddr>> {
    browse("_matterc._udp.local.", timeout, |info| {
        info.get_property_val_str("D")
            .and_then(|d| d.parse::<u16>().ok())
            .is_some_and(|d| discriminator.matches(d))
    })
}

/// An enrolled device's current address.
pub fn find_operational(
    compressed_fabric_id: u64,
    node_id: u64,
    timeout: Duration,
) -> Result<Option<SocketAddr>> {
    let instance = operational_instance(compressed_fabric_id, node_id);
    browse("_matter._tcp.local.", timeout, |info| {
        info.get_fullname()
            .split('.')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(&instance))
    })
}

fn browse(
    service: &str,
    timeout: Duration,
    wanted: impl Fn(&ServiceInfo) -> bool,
) -> Result<Option<SocketAddr>> {
    let daemon = ServiceDaemon::new().context("starting mDNS")?;
    let receiver = daemon.browse(service).context("browsing mDNS")?;
    let deadline = Instant::now() + timeout;
    let mut found = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = receiver.recv_timeout(left) else {
            break;
        };
        if let ServiceEvent::ServiceResolved(info) = event {
            if wanted(&info) {
                found = pick_address(&info);
                if found.is_some() {
                    break;
                }
            }
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}

/// Prefers IPv4, then routable IPv6. Link-local IPv6 needs a scope the
/// browser does not report, so it is skipped.
fn pick_address(info: &ServiceInfo) -> Option<SocketAddr> {
    let port = info.get_port();
    let mut addrs: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
    addrs.sort_by_key(|ip| match ip {
        IpAddr::V4(_) => 0,
        IpAddr::V6(v6) if !v6.is_unicast_link_local() => 1,
        IpAddr::V6(_) => 2,
    });
    addrs
        .into_iter()
        .find(|ip| !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
        .map(|ip| SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminators_and_instance_names() {
        assert!(Discriminator::Long(0xABC).matches(0xABC));
        assert!(Discriminator::Short(0xA).matches(0xABC));
        assert!(!Discriminator::Short(0xB).matches(0xABC));
        assert_eq!(
            operational_instance(0x87E1B004E235A130, 0x8FC7772401CD0696),
            "87E1B004E235A130-8FC7772401CD0696"
        );
    }
}
