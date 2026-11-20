//! TCP and UDP forwarding.

mod pktinfo;
mod tcp;
mod udp;

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use log::{debug, info, warn};
use socket2::{Domain, Socket, Type};
use thiserror::Error;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinSet;

use crate::spec::{Binding, ForwardSpec, Protocol};

const LISTEN_BACKLOG: i32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardConfig {
    /// Per-direction copy buffer for each TCP connection. Kept small on
    /// purpose: kernel socket buffers already absorb bursts, and large user
    /// space buffers waste memory and add latency on saturated links.
    pub tcp_buffer_size: usize,
    /// How long to wait for the target to accept a TCP connection.
    pub tcp_connect_timeout: Duration,
    /// Idle time before the first TCP keepalive probe.
    pub tcp_keepalive_time: Duration,
    /// Interval between unanswered TCP keepalive probes.
    pub tcp_keepalive_interval: Duration,
    /// Unanswered probes after which the kernel drops the connection.
    pub tcp_keepalive_retries: u32,
    /// UDP sessions without traffic in either direction for this long are
    /// dropped. TCP connections have no idle timeout at all: a connection is
    /// only closed by its endpoints, by errors or by failed keepalives.
    pub udp_idle_timeout: Duration,
    /// How often to retry binding sockets whose port was in use.
    pub bind_retry_interval: Duration,
}

impl Default for ForwardConfig {
    fn default() -> Self {
        Self {
            tcp_buffer_size: 16 * 1024,
            tcp_connect_timeout: Duration::from_secs(10),
            tcp_keepalive_time: Duration::from_secs(180),
            tcp_keepalive_interval: Duration::from_secs(30),
            tcp_keepalive_retries: 3,
            udp_idle_timeout: Duration::from_secs(600),
            bind_retry_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Error)]
#[error("failed to bind {binding}")]
pub struct BindError {
    pub binding: Binding,
    #[source]
    pub source: io::Error,
}

impl BindError {
    /// The port is taken by another socket; the only bind failure that is
    /// retried instead of being fatal.
    pub fn is_addr_in_use(&self) -> bool {
        self.source.kind() == io::ErrorKind::AddrInUse
    }
}

/// Why [`serve`] stopped.
#[derive(Debug, Error)]
pub enum ServeError {
    /// Retrying a binding failed for a reason other than the port being in use.
    #[error(transparent)]
    Bind(#[from] BindError),
    /// A bound listening socket stopped working, e.g. it was killed.
    #[error("lost listening socket {binding}")]
    ListenerLost {
        binding: Binding,
        #[source]
        source: io::Error,
    },
}

enum ListenSocket {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

/// A bound listening socket together with its forwarding target.
pub struct BoundListener {
    binding: Binding,
    socket: ListenSocket,
}

impl BoundListener {
    /// The binding with the actually bound local address (resolves port 0).
    pub fn binding(&self) -> Binding {
        self.binding
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.binding.local
    }

    pub fn protocol(&self) -> Protocol {
        self.binding.protocol
    }

    pub fn target(&self) -> SocketAddr {
        self.binding.target
    }
}

impl AsRawFd for BoundListener {
    fn as_raw_fd(&self) -> RawFd {
        match &self.socket {
            ListenSocket::Tcp(socket) => socket.as_raw_fd(),
            ListenSocket::Udp(socket) => socket.as_raw_fd(),
        }
    }
}

/// Result of [`bind_all`]: bound listeners plus bindings whose port was in
/// use and that [`serve`] keeps retrying.
pub struct Bound {
    pub listeners: Vec<BoundListener>,
    pub in_use: Vec<Binding>,
}

/// Binds a single listening socket. Must be called within a Tokio runtime.
///
/// IPv6 sockets are always IPv6-only so `[::]` never captures IPv4 traffic and
/// `*` can bind `0.0.0.0` and `[::]` side by side.
pub fn bind(binding: Binding) -> Result<BoundListener, BindError> {
    let wrap = |source| BindError { binding, source };
    let socket = match binding.protocol {
        Protocol::Tcp => ListenSocket::Tcp(bind_tcp(binding.local).map_err(wrap)?),
        Protocol::Udp => ListenSocket::Udp(bind_udp(binding.local).map_err(wrap)?),
    };
    let local = match &socket {
        ListenSocket::Tcp(l) => l.local_addr(),
        ListenSocket::Udp(s) => s.local_addr(),
    }
    .map_err(wrap)?;
    Ok(BoundListener {
        binding: Binding { local, ..binding },
        socket,
    })
}

/// Binds every socket described by `specs`. A port that is in use is not an
/// error: the binding is returned in [`Bound::in_use`] (with a warning) and
/// retried by [`serve`]. Any other failure is returned as an error, closing
/// everything bound so far. Must be called within a Tokio runtime. Run
/// [`crate::conflict::check`] first to rule out collisions between the specs.
pub fn bind_all(specs: &[ForwardSpec], config: &ForwardConfig) -> Result<Bound, BindError> {
    let mut bound = Bound {
        listeners: Vec::new(),
        in_use: Vec::new(),
    };
    for spec in specs {
        for binding in spec.bindings() {
            match bind(binding) {
                Ok(listener) => {
                    if binding.local.port() == 0 {
                        info!("listening on {} (ephemeral port)", listener.binding());
                    } else {
                        debug!("listening on {}", listener.binding());
                    }
                    bound.listeners.push(listener);
                }
                Err(err) if err.is_addr_in_use() => bound.in_use.push(binding),
                Err(err) => return Err(err),
            }
        }
    }
    if !bound.in_use.is_empty() {
        warn!(
            "port already in use, retrying every {}: {}",
            format_interval(config.bind_retry_interval),
            describe_bindings(&bound.in_use)
        );
        if bound
            .in_use
            .iter()
            .any(|b| crate::sys::is_unreserved_ephemeral_port(b.local.port()))
        {
            warn!(
                "some of these ports are in the kernel's ephemeral port range and may be held \
                 briefly by outgoing connections; consider net.ipv4.ip_local_reserved_ports"
            );
        }
    }
    Ok(bound)
}

fn format_interval(interval: Duration) -> String {
    if interval.as_secs() > 0 && interval.subsec_nanos() == 0 {
        format!("{}s", interval.as_secs())
    } else {
        format!("{interval:?}")
    }
}

/// Compact description of many bindings: consecutive ports with the same
/// protocol, addresses and consecutive targets are merged into ranges, e.g.
/// `tcp 0.0.0.0:80-82 -> 10.0.0.1:8080-8082, udp [::]:53 -> 10.0.0.1:53`.
pub fn describe_bindings(bindings: &[Binding]) -> String {
    // (protocol, local ip, target ip) -> ascending (local port, target port)
    let mut groups: BTreeMap<(Protocol, IpAddr, IpAddr), Vec<(u16, u16)>> = BTreeMap::new();
    for b in bindings {
        groups
            .entry((b.protocol, b.local.ip(), b.target.ip()))
            .or_default()
            .push((b.local.port(), b.target.port()));
    }
    let fmt_range = |ip: IpAddr, start: u16, end: u16| {
        let ports = if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        };
        match ip {
            IpAddr::V4(v4) => format!("{v4}:{ports}"),
            IpAddr::V6(v6) => format!("[{v6}]:{ports}"),
        }
    };
    let mut parts = Vec::new();
    for ((protocol, local_ip, target_ip), mut ports) in groups {
        ports.sort_unstable();
        let mut run: Option<(u16, u16, u16, u16)> = None;
        let mut flush = |run: (u16, u16, u16, u16)| {
            parts.push(format!(
                "{protocol} {} -> {}",
                fmt_range(local_ip, run.0, run.1),
                fmt_range(target_ip, run.2, run.3)
            ));
        };
        for (local, target) in ports {
            run = match run {
                Some((ls, le, ts, te))
                    if u32::from(le) + 1 == u32::from(local)
                        && u32::from(te) + 1 == u32::from(target) =>
                {
                    Some((ls, local, ts, target))
                }
                Some(previous) => {
                    flush(previous);
                    Some((local, local, target, target))
                }
                None => Some((local, local, target, target)),
            };
        }
        if let Some(run) = run {
            flush(run);
        }
    }
    parts.join(", ")
}

fn spawn_listener(
    tasks: &mut JoinSet<Result<(), ServeError>>,
    listener: BoundListener,
    config: ForwardConfig,
) {
    let binding = listener.binding;
    let lost = move |source| ServeError::ListenerLost { binding, source };
    match listener.socket {
        ListenSocket::Tcp(socket) => {
            tasks.spawn(async move { Err(lost(tcp::run(socket, binding.target, config).await)) });
        }
        ListenSocket::Udp(socket) => {
            tasks.spawn(async move { Err(lost(udp::run(socket, binding, config).await)) });
        }
    }
}

/// Forwards traffic for all bound listeners and keeps retrying in-use
/// bindings every `bind_retry_interval`. Runs until the future is dropped or
/// fails: losing any listening socket, or a retry failing for any reason
/// other than the port still being in use, stops everything with an error.
pub async fn serve(bound: Bound, config: ForwardConfig) -> Result<(), ServeError> {
    let mut tasks = JoinSet::new();
    for listener in bound.listeners {
        spawn_listener(&mut tasks, listener, config);
    }
    let mut in_use = bound.in_use;
    let mut retry = tokio::time::interval_at(
        tokio::time::Instant::now() + config.bind_retry_interval,
        config.bind_retry_interval,
    );
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some(result) = tasks.join_next() => match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                Err(_) => {}
            },
            _ = retry.tick(), if !in_use.is_empty() => {
                let mut newly_bound = Vec::new();
                let mut still_in_use = Vec::new();
                for binding in in_use {
                    match bind(binding) {
                        Ok(listener) => {
                            newly_bound.push(listener.binding());
                            spawn_listener(&mut tasks, listener, config);
                        }
                        Err(err) if err.is_addr_in_use() => still_in_use.push(binding),
                        Err(err) => return Err(err.into()),
                    }
                }
                if !newly_bound.is_empty() {
                    info!("bound after retry: {}", describe_bindings(&newly_bound));
                }
                if !still_in_use.is_empty() {
                    warn!(
                        "port still in use, retrying in {}: {}",
                        format_interval(config.bind_retry_interval),
                        describe_bindings(&still_in_use)
                    );
                }
                in_use = still_in_use;
            }
            else => return Ok(()),
        }
    }
}

fn new_socket(addr: SocketAddr, ty: Type) -> io::Result<Socket> {
    let socket = Socket::new(Domain::for_address(addr), ty, None)?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn bind_tcp(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = new_socket(addr, Type::STREAM)?;
    // Only allows rebinding over TIME_WAIT leftovers of our own previous
    // listener; Linux still refuses a port another socket is listening on.
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    TcpListener::from_std(socket.into())
}

fn bind_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = new_socket(addr, Type::DGRAM)?;
    if addr.ip().is_unspecified() {
        pktinfo::enable(&socket, addr.is_ipv6())?;
    }
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}
