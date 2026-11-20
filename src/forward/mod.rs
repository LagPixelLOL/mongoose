//! TCP and UDP forwarding.

mod pktinfo;
mod tcp;
mod udp;

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use log::{debug, info};
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

/// Binds every socket described by `specs`, failing on the first error (all
/// sockets bound so far are closed again). Must be called within a Tokio
/// runtime. Run [`crate::conflict::check`] first to rule out collisions.
pub fn bind_all(specs: &[ForwardSpec]) -> Result<Vec<BoundListener>, BindError> {
    let mut listeners = Vec::new();
    for spec in specs {
        for binding in spec.bindings() {
            let listener = bind(binding)?;
            if binding.local.port() == 0 {
                info!("listening on {} (ephemeral port)", listener.binding());
            } else {
                debug!("listening on {}", listener.binding());
            }
            listeners.push(listener);
        }
    }
    Ok(listeners)
}

/// Forwards traffic for all listeners. Runs until the future is dropped.
pub async fn serve(listeners: Vec<BoundListener>, config: ForwardConfig) {
    let mut tasks = JoinSet::new();
    for listener in listeners {
        let binding = listener.binding;
        match listener.socket {
            ListenSocket::Tcp(socket) => {
                tasks.spawn(tcp::run(socket, binding.target, config));
            }
            ListenSocket::Udp(socket) => {
                tasks.spawn(udp::run(socket, binding, config));
            }
        }
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(err) = result
            && err.is_panic()
        {
            std::panic::resume_unwind(err.into_panic());
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
    // Only allows rebinding over TIME_WAIT leftovers after a restart; Linux
    // still refuses to bind a port another socket is listening on.
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
