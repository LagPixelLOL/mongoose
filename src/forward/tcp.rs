//! TCP forwarding.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use log::{debug, warn};
use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use super::ForwardConfig;

pub(super) async fn run(listener: TcpListener, target: SocketAddr, config: ForwardConfig) {
    let local = listener.local_addr().ok();
    // Owned here so that dropping the listener task closes its connections.
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        match listener.accept().await {
            Ok((stream, peer)) => {
                connections.spawn(handle(stream, peer, target, config));
            }
            Err(err) if is_transient_accept_error(&err) => {
                debug!("tcp accept on {local:?}: {err}");
            }
            Err(err) => {
                // Typically EMFILE/ENFILE/ENOBUFS: back off instead of spinning.
                warn!("tcp accept on {local:?} failed: {err}");
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

fn configure(stream: &TcpStream, config: &ForwardConfig) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let keepalive = TcpKeepalive::new()
        .with_time(config.tcp_keepalive_time)
        .with_interval(config.tcp_keepalive_interval)
        .with_retries(config.tcp_keepalive_retries);
    SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// Makes closing the stream send an RST instead of a FIN.
fn reset_on_close(stream: &TcpStream) {
    let _ = stream.set_zero_linger();
}

async fn handle(inbound: TcpStream, peer: SocketAddr, target: SocketAddr, config: ForwardConfig) {
    if let Err(err) = configure(&inbound, &config) {
        debug!("tcp {peer}: cannot configure socket: {err}");
        reset_on_close(&inbound);
        return;
    }
    let outbound =
        match tokio::time::timeout(config.tcp_connect_timeout, TcpStream::connect(target)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(err)) => {
                warn!("tcp {peer} -> {target}: connect failed: {err}");
                // Mirror the failure to the client as a reset, not a clean close.
                reset_on_close(&inbound);
                return;
            }
            Err(_) => {
                warn!(
                    "tcp {peer} -> {target}: connect timed out after {:?}",
                    config.tcp_connect_timeout
                );
                reset_on_close(&inbound);
                return;
            }
        };
    if let Err(err) = configure(&outbound, &config) {
        debug!("tcp {peer} -> {target}: cannot configure socket: {err}");
        reset_on_close(&inbound);
        reset_on_close(&outbound);
        return;
    }
    debug!("tcp {peer} -> {target}: connected");
    let (mut inbound, mut outbound) = (inbound, outbound);
    let size = config.tcp_buffer_size;
    // Half-closes are propagated (EOF on one side shuts down writing on the
    // other) and the connection lives until both directions are done.
    match tokio::io::copy_bidirectional_with_sizes(&mut inbound, &mut outbound, size, size).await {
        Ok((sent, received)) => {
            debug!("tcp {peer} -> {target}: closed ({sent} bytes sent, {received} bytes received)")
        }
        Err(err) => {
            debug!("tcp {peer} -> {target}: aborted: {err}");
            // A reset or failure on one side is passed on as a reset.
            reset_on_close(&inbound);
            reset_on_close(&outbound);
        }
    }
}
