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

use std::collections::{BTreeMap, HashMap};
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
    /// Colliding local ports, merged into ranges.
    pub ports: Vec<PortRange>,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ports: Vec<String> = self.ports.iter().map(ToString::to_string).collect();
        write!(
            f,
            "rule #{} '{}' and rule #{} '{}' both bind {} port {} ({} vs {})",
            self.first + 1,
            self.first_spec,
            self.second + 1,
            self.second_spec,
            self.protocol,
            ports.join(","),
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
        write!(f, "{} conflicting forwarding rule pair(s):", self.0.len())?;
        for conflict in &self.0 {
            write!(f, "\n  {conflict}")?;
        }
        Ok(())
    }
}

fn overlaps(a: IpAddr, b: IpAddr) -> bool {
    a == b || a.is_unspecified() || b.is_unspecified()
}

/// Returns every collision between `specs`, or `Ok(())` if there is none.
pub fn check(specs: &[ForwardSpec]) -> Result<(), ConflictError> {
    // (protocol, is_ipv6, port) -> bound (ip, spec index)
    let mut bound: HashMap<(Protocol, bool, u16), Vec<(IpAddr, usize)>> = HashMap::new();
    // (first, second, protocol, first ip, second ip) -> ports, in ascending order
    let mut found: BTreeMap<(usize, usize, Protocol, IpAddr, IpAddr), Vec<u16>> = BTreeMap::new();

    for (index, spec) in specs.iter().enumerate() {
        for binding in spec.bindings() {
            let port = binding.local.port();
            if port == 0 {
                continue;
            }
            let ip = binding.local.ip();
            let entry = bound
                .entry((binding.protocol, ip.is_ipv6(), port))
                .or_default();
            for &(other_ip, other) in entry.iter() {
                if other != index && overlaps(ip, other_ip) {
                    found
                        .entry((other, index, binding.protocol, other_ip, ip))
                        .or_default()
                        .push(port);
                }
            }
            entry.push((ip, index));
        }
    }

    if found.is_empty() {
        return Ok(());
    }
    let conflicts = found
        .into_iter()
        .map(
            |((first, second, protocol, first_ip, second_ip), ports)| Conflict {
                first,
                second,
                first_spec: specs[first],
                second_spec: specs[second],
                protocol,
                first_ip,
                second_ip,
                ports: merge_ports(&ports),
            },
        )
        .collect();
    Err(ConflictError(conflicts))
}

/// Merges ascending ports into inclusive ranges.
fn merge_ports(ports: &[u16]) -> Vec<PortRange> {
    let mut ranges: Vec<PortRange> = Vec::new();
    for &port in ports {
        match ranges.last_mut() {
            Some(last) if u32::from(last.end()) + 1 == u32::from(port) => {
                *last = PortRange::new(last.start(), port).expect("ascending");
            }
            Some(last) if last.end() >= port => {}
            _ => ranges.push(PortRange::single(port)),
        }
    }
    ranges
}
