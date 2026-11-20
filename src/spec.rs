//! Parsing of forwarding specifications.
//!
//! Format: `[tcp:|udp:]LOCAL_IP:LOCAL_PORT:TARGET_IP:TARGET_PORT`
//!
//! * Without a protocol prefix both TCP and UDP are forwarded.
//! * `LOCAL_IP` may be `*`, which binds both `0.0.0.0` and `[::]`.
//! * IPv6 addresses must be wrapped in brackets, e.g. `[::1]`.
//! * Ports are either a single number or an inclusive range `START-END`.
//!   Port `0` (ephemeral) is only allowed as a single local port.
//! * A single target port combined with a local range means a range of the
//!   same length starting at that port.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

use thiserror::Error;

/// Transport protocol of a single forwarding binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        })
    }
}

/// Which protocols a specification forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocols {
    Tcp,
    Udp,
    Both,
}

impl Protocols {
    pub fn list(self) -> &'static [Protocol] {
        match self {
            Protocols::Tcp => &[Protocol::Tcp],
            Protocols::Udp => &[Protocol::Udp],
            Protocols::Both => &[Protocol::Tcp, Protocol::Udp],
        }
    }
}

/// Local address to bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalAddr {
    /// `*`: bind both `0.0.0.0` and `[::]`.
    Any,
    Ip(IpAddr),
}

impl LocalAddr {
    pub fn ips(self) -> Vec<IpAddr> {
        match self {
            LocalAddr::Any => vec![
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            ],
            LocalAddr::Ip(ip) => vec![ip],
        }
    }
}

impl fmt::Display for LocalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocalAddr::Any => f.write_str("*"),
            LocalAddr::Ip(ip) => fmt_ip(f, *ip),
        }
    }
}

fn fmt_ip(f: &mut fmt::Formatter<'_>, ip: IpAddr) -> fmt::Result {
    match ip {
        IpAddr::V4(v4) => write!(f, "{v4}"),
        IpAddr::V6(v6) => write!(f, "[{v6}]"),
    }
}

/// An inclusive port range. A single port is a range with `start == end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    pub fn new(start: u16, end: u16) -> Option<Self> {
        (start <= end).then_some(Self { start, end })
    }

    pub fn single(port: u16) -> Self {
        Self {
            start: port,
            end: port,
        }
    }

    pub fn start(self) -> u16 {
        self.start
    }

    pub fn end(self) -> u16 {
        self.end
    }

    /// Number of ports in the range (always at least 1).
    pub fn count(self) -> u32 {
        u32::from(self.end) - u32::from(self.start) + 1
    }

    pub fn is_single(self) -> bool {
        self.start == self.end
    }

    pub fn iter(self) -> impl Iterator<Item = u16> {
        self.start..=self.end
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_single() {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

/// One concrete socket to listen on and where to forward its traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    pub protocol: Protocol,
    pub local: SocketAddr,
    pub target: SocketAddr,
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} -> {}", self.protocol, self.local, self.target)
    }
}

/// A parsed forwarding specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForwardSpec {
    pub protocols: Protocols,
    pub local_addr: LocalAddr,
    pub local_ports: PortRange,
    pub target_ip: IpAddr,
    pub target_ports: PortRange,
}

impl ForwardSpec {
    /// Expands the specification into every concrete socket binding.
    pub fn bindings(&self) -> Vec<Binding> {
        let mut out = Vec::new();
        for &protocol in self.protocols.list() {
            for ip in self.local_addr.ips() {
                for (local_port, target_port) in
                    self.local_ports.iter().zip(self.target_ports.iter())
                {
                    out.push(Binding {
                        protocol,
                        local: SocketAddr::new(ip, local_port),
                        target: SocketAddr::new(self.target_ip, target_port),
                    });
                }
            }
        }
        out
    }
}

impl fmt::Display for ForwardSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.protocols {
            Protocols::Tcp => f.write_str("tcp:")?,
            Protocols::Udp => f.write_str("udp:")?,
            Protocols::Both => {}
        }
        write!(f, "{}:{}:", self.local_addr, self.local_ports)?;
        fmt_ip(f, self.target_ip)?;
        write!(f, ":{}", self.target_ports)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SpecError {
    #[error("expected [tcp:|udp:]LOCAL_IP:LOCAL_PORT:TARGET_IP:TARGET_PORT")]
    Format,
    #[error("unknown protocol '{0}' (expected tcp or udp)")]
    UnknownProtocol(String),
    #[error("invalid {role} address '{value}'")]
    InvalidAddress { role: &'static str, value: String },
    #[error("IPv6 addresses must be wrapped in brackets, e.g. [::1]")]
    UnbracketedIpv6,
    #[error("only IPv6 addresses may be wrapped in brackets, got '[{0}]'")]
    BracketedNonIpv6(String),
    #[error("'*' is not allowed as the target address")]
    WildcardTarget,
    #[error("invalid {role} port '{value}'")]
    InvalidPort { role: &'static str, value: String },
    #[error("invalid {role} port range '{value}': start must not be greater than end")]
    ReversedRange { role: &'static str, value: String },
    #[error("port 0 is not allowed in a {role} port range")]
    ZeroInRange { role: &'static str },
    #[error("target port 0 is not allowed")]
    ZeroTargetPort,
    #[error("target port range has {target} ports but local range has {local}")]
    RangeLengthMismatch { local: u32, target: u32 },
    #[error("target port range starting at {start} with {len} ports exceeds 65535")]
    TargetRangeOverflow { start: u16, len: u32 },
}

impl FromStr for ForwardSpec {
    type Err = SpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse(s)
    }
}

/// Parses a forwarding specification such as `tcp:*:8080:[::1]:80`.
pub fn parse(input: &str) -> Result<ForwardSpec, SpecError> {
    check_unbracketed_ipv6(input)?;
    let (protocols, rest) = split_protocol(input)?;

    let (local_host, rest) = take_host(rest)?;
    let local_addr = if local_host.text == "*" && !local_host.bracketed {
        LocalAddr::Any
    } else {
        LocalAddr::Ip(parse_ip(&local_host, "local")?)
    };

    let (local_port_text, rest) = rest.split_once(':').ok_or(SpecError::Format)?;
    let local_ports = parse_ports(local_port_text, "local")?;

    let (target_host, target_port_text) = take_host(rest)?;
    if target_host.text == "*" && !target_host.bracketed {
        return Err(SpecError::WildcardTarget);
    }
    let target_ip = parse_ip(&target_host, "target")?;
    if target_port_text.contains(':') {
        return Err(SpecError::Format);
    }

    let target = parse_ports(target_port_text, "target")?;
    if target.start() == 0 {
        return Err(SpecError::ZeroTargetPort);
    }
    let target_ports = if target.is_single() {
        let len = local_ports.count();
        let end = u32::from(target.start()) + len - 1;
        let end = u16::try_from(end).map_err(|_| SpecError::TargetRangeOverflow {
            start: target.start(),
            len,
        })?;
        PortRange::new(target.start(), end).expect("end >= start")
    } else if target.count() != local_ports.count() {
        return Err(SpecError::RangeLengthMismatch {
            local: local_ports.count(),
            target: target.count(),
        });
    } else {
        target
    };

    Ok(ForwardSpec {
        protocols,
        local_addr,
        local_ports,
        target_ip,
        target_ports,
    })
}

fn split_protocol(input: &str) -> Result<(Protocols, &str), SpecError> {
    if let Some((head, rest)) = input.split_once(':') {
        if head.eq_ignore_ascii_case("tcp") {
            return Ok((Protocols::Tcp, rest));
        }
        if head.eq_ignore_ascii_case("udp") {
            return Ok((Protocols::Udp, rest));
        }
        // Something alphabetic that is not an IPv4 address or `*` can only be
        // an attempt at a protocol prefix.
        if !head.is_empty() && head.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(SpecError::UnknownProtocol(head.to_string()));
        }
    }
    Ok((Protocols::Both, input))
}

/// A well formed spec has at most 4 colons outside of brackets (protocol,
/// local ip, local port, target ip). More than that, or a `::` outside of
/// brackets, means an IPv6 address was written without brackets.
fn check_unbracketed_ipv6(input: &str) -> Result<(), SpecError> {
    let mut depth = 0usize;
    let mut colons = 0usize;
    let mut prev_colon = false;
    for c in input.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => {
                if prev_colon {
                    return Err(SpecError::UnbracketedIpv6);
                }
                colons += 1;
            }
            _ => {}
        }
        prev_colon = c == ':' && depth == 0;
    }
    if colons > 4 {
        return Err(SpecError::UnbracketedIpv6);
    }
    Ok(())
}

struct Host<'a> {
    text: &'a str,
    bracketed: bool,
}

/// Splits `HOST:REST` where HOST may be a bracketed IPv6 address.
fn take_host<'a>(input: &'a str) -> Result<(Host<'a>, &'a str), SpecError> {
    if let Some(inner) = input.strip_prefix('[') {
        let (text, after) = inner.split_once(']').ok_or(SpecError::Format)?;
        let rest = after.strip_prefix(':').ok_or(SpecError::Format)?;
        return Ok((
            Host {
                text,
                bracketed: true,
            },
            rest,
        ));
    }
    let (text, rest) = input.split_once(':').ok_or(SpecError::Format)?;
    if text.is_empty() {
        return Err(SpecError::Format);
    }
    Ok((
        Host {
            text,
            bracketed: false,
        },
        rest,
    ))
}

fn parse_ip(host: &Host<'_>, role: &'static str) -> Result<IpAddr, SpecError> {
    if host.bracketed {
        return match host.text.parse::<Ipv6Addr>() {
            Ok(v6) => Ok(IpAddr::V6(v6)),
            Err(_) if host.text.parse::<Ipv4Addr>().is_ok() => {
                Err(SpecError::BracketedNonIpv6(host.text.to_string()))
            }
            Err(_) => Err(SpecError::InvalidAddress {
                role,
                value: format!("[{}]", host.text),
            }),
        };
    }
    host.text
        .parse::<Ipv4Addr>()
        .map(IpAddr::V4)
        .map_err(|_| SpecError::InvalidAddress {
            role,
            value: host.text.to_string(),
        })
}

fn parse_port(text: &str, role: &'static str) -> Result<u16, SpecError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SpecError::InvalidPort {
            role,
            value: text.to_string(),
        });
    }
    text.parse::<u16>().map_err(|_| SpecError::InvalidPort {
        role,
        value: text.to_string(),
    })
}

fn parse_ports(text: &str, role: &'static str) -> Result<PortRange, SpecError> {
    match text.split_once('-') {
        None => Ok(PortRange::single(parse_port(text, role)?)),
        Some((start, end)) => {
            let start = parse_port(start, role)?;
            let end_port = parse_port(end, role)?;
            if start == 0 || end_port == 0 {
                return Err(SpecError::ZeroInRange { role });
            }
            PortRange::new(start, end_port).ok_or_else(|| SpecError::ReversedRange {
                role,
                value: text.to_string(),
            })
        }
    }
}
