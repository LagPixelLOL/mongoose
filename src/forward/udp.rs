//! UDP forwarding.
//!
//! Every client address gets its own connected upstream socket (a "session")
//! so replies from the target can be routed back. Sessions expire after
//! `udp_idle_timeout` without traffic in either direction.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, warn};
use tokio::io::Interest;
use tokio::net::UdpSocket;

use super::ForwardConfig;
use super::pktinfo::{self, LocalDst};
use crate::spec::Binding;

/// Largest possible UDP payload.
const MAX_DATAGRAM: usize = 65_535;

thread_local! {
    // One datagram buffer per runtime worker thread instead of one per socket
    // or session: port ranges can create thousands of UDP sockets.
    static DATAGRAM_BUF: RefCell<Box<[u8]>> =
        RefCell::new(vec![0u8; MAX_DATAGRAM].into_boxed_slice());
}

/// Sessions are keyed by client address and the local address it sent to.
type SessionKey = (SocketAddr, Option<IpAddr>);

struct UdpForwarder {
    socket: UdpSocket,
    target: SocketAddr,
    sessions: Mutex<HashMap<SessionKey, Arc<Session>>>,
    clock: Instant,
    idle_ms: u64,
}

struct Session {
    peer: SocketAddr,
    /// Address to send replies from (wildcard listeners only).
    source: Option<LocalDst>,
    upstream: UdpSocket,
    last_active_ms: AtomicU64,
}

impl UdpForwarder {
    fn now_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    fn touch(&self, session: &Session) {
        session
            .last_active_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    fn idle_for(&self, session: &Session) -> u64 {
        self.now_ms()
            .saturating_sub(session.last_active_ms.load(Ordering::Relaxed))
    }

    /// Returns the session for a client, creating it if needed. The activity
    /// timestamp is refreshed under the lock so a concurrent idle check cannot
    /// expire a session that is about to be used.
    fn session_for(
        self: &Arc<Self>,
        peer: SocketAddr,
        source: Option<LocalDst>,
    ) -> io::Result<Arc<Session>> {
        let key = (peer, source.map(|s| s.ip));
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(&key) {
            self.touch(session);
            return Ok(session.clone());
        }
        let session = Arc::new(Session {
            peer,
            source,
            upstream: connect_upstream(self.target)?,
            last_active_ms: AtomicU64::new(self.now_ms()),
        });
        sessions.insert(key, session.clone());
        debug!(
            "udp {peer} -> {}: new session via {:?}",
            self.target,
            session.upstream.local_addr()
        );
        tokio::spawn(run_replies(self.clone(), key, session.clone()));
        Ok(session)
    }

    /// Removes the session if it is still the one registered under `key`
    /// and, unless `force`, still idle. Returns whether it was expired.
    fn expire(&self, key: SessionKey, session: &Arc<Session>, force: bool) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if !force && self.idle_for(session) < self.idle_ms {
            return false;
        }
        if sessions.get(&key).is_some_and(|s| Arc::ptr_eq(s, session)) {
            sessions.remove(&key);
        }
        true
    }

    fn try_send_reply(&self, session: &Session, data: &[u8]) -> io::Result<usize> {
        let fd = self.socket.as_raw_fd();
        self.socket.try_io(Interest::WRITABLE, || {
            pktinfo::send_to(fd, data, session.peer, session.source)
        })
    }

    async fn send_reply(&self, session: &Session, data: &[u8]) -> io::Result<usize> {
        let fd = self.socket.as_raw_fd();
        self.socket
            .async_io(Interest::WRITABLE, || {
                pktinfo::send_to(fd, data, session.peer, session.source)
            })
            .await
    }
}

/// Sends on a connected upstream socket. An ICMP error from an earlier
/// datagram (e.g. port unreachable while the target restarts) is reported as
/// ECONNREFUSED by the next send *without* sending it, so retry once.
fn try_send_upstream(session: &Session, data: &[u8]) -> io::Result<usize> {
    match session.upstream.try_send(data) {
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            session.upstream.try_send(data)
        }
        result => result,
    }
}

async fn send_upstream(session: &Session, data: &[u8]) -> io::Result<usize> {
    match session.upstream.send(data).await {
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            session.upstream.send(data).await
        }
        result => result,
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
pub(super) async fn run(socket: UdpSocket, binding: Binding, config: ForwardConfig) {
    let fwd = Arc::new(UdpForwarder {
        socket,
        target: binding.target,
        sessions: Mutex::new(HashMap::new()),
        clock: Instant::now(),
        idle_ms: config.udp_idle_timeout.as_millis().max(1) as u64,
    });
    let fd = fwd.socket.as_raw_fd();
    loop {
        if let Err(err) = fwd.socket.readable().await {
            warn!("udp {}: {err}", binding.local);
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        // Fast path: receive and forward synchronously while borrowing the
        // shared buffer; only copy the datagram if the send would block.
        let pending = DATAGRAM_BUF.with_borrow_mut(|buf| {
            let received = fwd
                .socket
                .try_io(Interest::READABLE, || pktinfo::recv_from(fd, buf));
            let (len, peer, source) = match received {
                Ok(received) => received,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return None,
                Err(err) => {
                    debug!("udp {}: receive failed: {err}", binding.local);
                    return None;
                }
            };
            let session = match fwd.session_for(peer, source) {
                Ok(session) => session,
                Err(err) => {
                    warn!("udp {peer} -> {}: cannot create session: {err}", fwd.target);
                    return None;
                }
            };
            match try_send_upstream(&session, &buf[..len]) {
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
            && let Err(err) = send_upstream(&session, &data).await
        {
            debug!("udp {} -> {}: send failed: {err}", session.peer, fwd.target);
        }
    }
}

/// Target -> client direction for one session; also handles idle expiry.
async fn run_replies(fwd: Arc<UdpForwarder>, key: SessionKey, session: Arc<Session>) {
    let peer = session.peer;
    loop {
        let idle = fwd.idle_for(&session);
        if idle >= fwd.idle_ms {
            if fwd.expire(key, &session, false) {
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
                fwd.expire(key, &session, true);
                return;
            }
            Ok(Ok(())) => {}
        }
        let pending = DATAGRAM_BUF.with_borrow_mut(|buf| {
            let len = match session.upstream.try_recv(buf) {
                Ok(len) => len,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return None,
                Err(err) => {
                    // e.g. ICMP port unreachable reported as ECONNREFUSED;
                    // the target may come back, so keep the session.
                    debug!("udp {peer} -> {}: receive failed: {err}", fwd.target);
                    return None;
                }
            };
            fwd.touch(&session);
            match fwd.try_send_reply(&session, &buf[..len]) {
                Ok(_) => None,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => Some(buf[..len].to_vec()),
                Err(err) => {
                    debug!("udp {} -> {peer}: send failed: {err}", fwd.target);
                    None
                }
            }
        });
        if let Some(data) = pending
            && let Err(err) = fwd.send_reply(&session, &data).await
        {
            debug!("udp {} -> {peer}: send failed: {err}", fwd.target);
        }
    }
}
