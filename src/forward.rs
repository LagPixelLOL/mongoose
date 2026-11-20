//! TCP and UDP forwarding.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use socket2::{Domain, Socket, Type};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinSet;

use crate::spec::{Binding, ForwardSpec, Protocol};

/// Largest possible UDP payload.
const MAX_DATAGRAM: usize = 65_535;
const LISTEN_BACKLOG: i32 = 4096;

thread_local! {
    // One datagram buffer per runtime worker thread instead of one per socket
    // or session: port ranges can create thousands of UDP sockets.
    static DATAGRAM_BUF: RefCell<Box<[u8]>> =
        RefCell::new(vec![0u8; MAX_DATAGRAM].into_boxed_slice());
}

#[derive(Debug, Clone, Copy)]
pub struct ForwardConfig {
    /// UDP sessions without traffic in either direction for this long are dropped.
    pub udp_idle_timeout: Duration,
    /// Per-direction copy buffer for each TCP connection. Kept small on purpose:
    /// the kernel socket buffers already absorb bursts, and large user space
    /// buffers waste memory and add latency on saturated links.
    pub tcp_buffer_size: usize,
}

impl Default for ForwardConfig {
    fn default() -> Self {
        Self {
            udp_idle_timeout: Duration::from_secs(60),
            tcp_buffer_size: 16 * 1024,
        }
    }
}

#[derive(Debug, Error)]
#[error("failed to bind {binding}: {source}")]
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

/// Binds every socket described by `specs`. Must be called within a Tokio runtime.
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
        let target = listener.binding.target;
        match listener.socket {
            ListenSocket::Tcp(tcp) => {
                tasks.spawn(run_tcp(tcp, target, config.tcp_buffer_size));
            }
            ListenSocket::Udp(udp) => {
                let forwarder = Arc::new(UdpForwarder {
                    socket: udp,
                    target,
                    sessions: Mutex::new(HashMap::new()),
                    clock: Instant::now(),
                    idle_ms: config.udp_idle_timeout.as_millis().max(1) as u64,
                });
                tasks.spawn(run_udp(forwarder));
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
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    TcpListener::from_std(socket.into())
}

fn bind_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = new_socket(addr, Type::DGRAM)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

// ---------------------------------------------------------------------------
// TCP

async fn run_tcp(listener: TcpListener, target: SocketAddr, buffer_size: usize) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(handle_tcp(stream, peer, target, buffer_size));
            }
            Err(err) if is_transient_accept_error(&err) => {
                debug!("tcp accept on {:?}: {err}", listener.local_addr());
            }
            Err(err) => {
                // Typically EMFILE/ENFILE/ENOBUFS: back off instead of spinning.
                warn!("tcp accept on {:?} failed: {err}", listener.local_addr());
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn is_transient_accept_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::Interrupted
    )
}

async fn handle_tcp(mut inbound: TcpStream, peer: SocketAddr, target: SocketAddr, size: usize) {
    let _ = inbound.set_nodelay(true);
    let mut outbound = match TcpStream::connect(target).await {
        Ok(stream) => stream,
        Err(err) => {
            warn!("tcp {peer} -> {target}: connect failed: {err}");
            return;
        }
    };
    let _ = outbound.set_nodelay(true);
    debug!("tcp {peer} -> {target}: connected");
    match tokio::io::copy_bidirectional_with_sizes(&mut inbound, &mut outbound, size, size).await {
        Ok((sent, received)) => {
            debug!("tcp {peer} -> {target}: closed ({sent} bytes sent, {received} bytes received)")
        }
        Err(err) => debug!("tcp {peer} -> {target}: closed with error: {err}"),
    }
}

// ---------------------------------------------------------------------------
// UDP

struct UdpForwarder {
    socket: UdpSocket,
    target: SocketAddr,
    sessions: Mutex<HashMap<SocketAddr, Arc<UdpSession>>>,
    clock: Instant,
    idle_ms: u64,
}

struct UdpSession {
    upstream: UdpSocket,
    last_active_ms: AtomicU64,
}

impl UdpForwarder {
    fn now_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    fn touch(&self, session: &UdpSession) {
        session
            .last_active_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    fn idle_for(&self, session: &UdpSession) -> u64 {
        self.now_ms()
            .saturating_sub(session.last_active_ms.load(Ordering::Relaxed))
    }

    /// Returns the session for `peer`, creating it if needed. The activity
    /// timestamp is refreshed under the lock so a concurrent idle check cannot
    /// expire a session that is about to be used.
    fn session_for(self: &Arc<Self>, peer: SocketAddr) -> io::Result<Arc<UdpSession>> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(&peer) {
            self.touch(session);
            return Ok(session.clone());
        }
        let session = Arc::new(UdpSession {
            upstream: connect_upstream(self.target)?,
            last_active_ms: AtomicU64::new(self.now_ms()),
        });
        sessions.insert(peer, session.clone());
        debug!(
            "udp {peer} -> {}: new session via {:?}",
            self.target,
            session.upstream.local_addr()
        );
        tokio::spawn(run_udp_replies(self.clone(), peer, session.clone()));
        Ok(session)
    }

    /// Removes the session if it is still the one registered for `peer` and,
    /// unless `force`, it is still idle.
    fn expire(&self, peer: SocketAddr, session: &Arc<UdpSession>, force: bool) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if !force && self.idle_for(session) < self.idle_ms {
            return false;
        }
        if sessions.get(&peer).is_some_and(|s| Arc::ptr_eq(s, session)) {
            sessions.remove(&peer);
        }
        true
    }
}

fn connect_upstream(target: SocketAddr) -> io::Result<UdpSocket> {
    let local: SocketAddr = if target.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = std::net::UdpSocket::bind(local)?;
    socket.connect(target)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket)
}

/// Client -> target direction for one listening socket.
async fn run_udp(fwd: Arc<UdpForwarder>) {
    loop {
        if let Err(err) = fwd.socket.readable().await {
            warn!("udp {:?}: {err}", fwd.socket.local_addr());
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        // Fast path: receive and forward synchronously while borrowing the
        // shared buffer; only copy the datagram if the send would block.
        let pending = DATAGRAM_BUF.with_borrow_mut(|buf| {
            let (len, peer) = match fwd.socket.try_recv_from(buf) {
                Ok(received) => received,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return None,
                Err(err) => {
                    debug!("udp {:?}: receive failed: {err}", fwd.socket.local_addr());
                    return None;
                }
            };
            let session = match fwd.session_for(peer) {
                Ok(session) => session,
                Err(err) => {
                    warn!("udp {peer} -> {}: cannot create session: {err}", fwd.target);
                    return None;
                }
            };
            match session.upstream.try_send(&buf[..len]) {
                Ok(_) => None,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    Some((session, buf[..len].to_vec()))
                }
                Err(err) => {
                    debug!("udp {peer} -> {}: send failed: {err}", fwd.target);
                    None
                }
            }
        });
        if let Some((session, data)) = pending
            && let Err(err) = session.upstream.send(&data).await
        {
            debug!("udp -> {}: send failed: {err}", fwd.target);
        }
    }
}

/// Target -> client direction for one session; also handles idle expiry.
async fn run_udp_replies(fwd: Arc<UdpForwarder>, peer: SocketAddr, session: Arc<UdpSession>) {
    loop {
        let idle = fwd.idle_for(&session);
        if idle >= fwd.idle_ms {
            if fwd.expire(peer, &session, false) {
                debug!("udp {peer} -> {}: session expired", fwd.target);
                return;
            }
            continue;
        }
        let remaining = Duration::from_millis(fwd.idle_ms - idle);
        match tokio::time::timeout(remaining, session.upstream.readable()).await {
            Err(_) => continue,
            Ok(Err(err)) => {
                debug!("udp {peer} -> {}: session failed: {err}", fwd.target);
                fwd.expire(peer, &session, true);
                return;
            }
            Ok(Ok(())) => {}
        }
        let pending = DATAGRAM_BUF.with_borrow_mut(|buf| {
            let len = match session.upstream.try_recv(buf) {
                Ok(len) => len,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return None,
                Err(err) => {
                    // e.g. ICMP port unreachable reported as ECONNREFUSED.
                    debug!("udp {peer} -> {}: receive failed: {err}", fwd.target);
                    return None;
                }
            };
            fwd.touch(&session);
            match fwd.socket.try_send_to(&buf[..len], peer) {
                Ok(_) => None,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => Some(buf[..len].to_vec()),
                Err(err) => {
                    debug!("udp {} -> {peer}: send failed: {err}", fwd.target);
                    None
                }
            }
        });
        if let Some(data) = pending
            && let Err(err) = fwd.socket.send_to(&data, peer).await
        {
            debug!("udp {} -> {peer}: send failed: {err}", fwd.target);
        }
    }
}
