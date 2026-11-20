//! Collision detection between forwarding specifications.
//!
//! Runs purely on the parsed specs, before any socket is created, so every
//! collision is reported up front instead of surfacing as a bind error or
//! being resolved by some implicit precedence rule.
//!
//! Two bindings collide when they use the same protocol and port and their
//! local addresses either are equal or one of them is the unspecified address
//! of the same family (`0.0.0.0` covers all IPv4 addresses, `[::]` all IPv6
//! addresses; IPv6 sockets are IPv6-only so the families never overlap).
//! Port 0 requests an ephemeral port and never collides.

use std::fmt;
use std::net::IpAddr;

use thiserror::Error;

use crate::spec::{ForwardSpec, PortRange, Protocol};

/// A collision between two specs (indices into the checked slice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub first: usize,
    pub second: usize,
    pub first_spec: ForwardSpec,
    pub second_spec: ForwardSpec,
    pub protocol: Protocol,
    pub first_ip: IpAddr,
    pub second_ip: IpAddr,
    /// Colliding local ports.
    pub ports: PortRange,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rule #{} '{}' and rule #{} '{}' both bind {} port {} ({} vs {})",
            self.first + 1,
            self.first_spec,
            self.second + 1,
            self.second_spec,
            self.protocol,
            self.ports,
            DisplayIp(self.first_ip),
            DisplayIp(self.second_ip),
        )
    }
}

struct DisplayIp(IpAddr);

impl fmt::Display for DisplayIp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddr::V4(v4) => write!(f, "{v4}"),
            IpAddr::V6(v6) => write!(f, "[{v6}]"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub struct ConflictError(pub Vec<Conflict>);

impl fmt::Display for ConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} conflict(s) between forwarding rules:", self.0.len())?;
        for conflict in &self.0 {
            write!(f, "\n  {conflict}")?;
        }
        Ok(())
    }
}

fn overlaps(a: IpAddr, b: IpAddr) -> bool {
    a == b || a.is_unspecified() || b.is_unspecified()
}

/// Port range used by both specs, if any. Ephemeral port 0 never collides.
fn shared_ports(a: PortRange, b: PortRange) -> Option<PortRange> {
    if a.start() == 0 || b.start() == 0 {
        return None;
    }
    PortRange::new(a.start().max(b.start()), a.end().min(b.end()))
}

/// Returns every collision between `specs`, or `Ok(())` if there is none.
///
/// Each spec covers one contiguous port range, so collisions are found by
/// intersecting ranges pairwise instead of expanding individual ports.
pub fn check(specs: &[ForwardSpec]) -> Result<(), ConflictError> {
    let mut conflicts = Vec::new();
    for (first, a) in specs.iter().enumerate() {
        for (offset, b) in specs[first + 1..].iter().enumerate() {
            let Some(ports) = shared_ports(a.local_ports, b.local_ports) else {
                continue;
            };
            for &protocol in a.protocols.list() {
                if !b.protocols.list().contains(&protocol) {
                    continue;
                }
                for first_ip in a.local_addr.ips() {
                    for second_ip in b.local_addr.ips() {
                        if first_ip.is_ipv6() == second_ip.is_ipv6()
                            && overlaps(first_ip, second_ip)
                        {
                            conflicts.push(Conflict {
                                first,
                                second: first + 1 + offset,
                                first_spec: *a,
                                second_spec: *b,
                                protocol,
                                first_ip,
                                second_ip,
                                ports,
                            });
                        }
                    }
                }
            }
        }
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(ConflictError(conflicts))
    }
}
