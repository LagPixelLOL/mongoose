use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use mongoose::forward::{self, BoundListener, ForwardConfig};
use mongoose::spec::{ForwardSpec, Protocol};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

fn spec(text: &str) -> ForwardSpec {
    text.parse().unwrap()
}

fn ipv6_loopback_available() -> bool {
    std::net::UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// Binds the specs and serves them in the background, returning the bound
/// listeners' bindings.
fn start(specs: &[ForwardSpec], config: ForwardConfig) -> Vec<mongoose::spec::Binding> {
    let listeners = forward::bind_all(specs).unwrap();
    let bindings = listeners.iter().map(BoundListener::binding).collect();
    tokio::spawn(forward::serve(listeners, config));
    bindings
}

fn first(bindings: &[mongoose::spec::Binding], protocol: Protocol) -> SocketAddr {
    bindings
        .iter()
        .find(|b| b.protocol == protocol)
        .unwrap()
        .local
}

/// Echo server that reads until EOF on each connection, echoing as it goes.
async fn tcp_echo(addr: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind(addr).await.unwrap();
    let local = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
                let _ = write.shutdown().await;
            });
        }
    });
    local
}

/// UDP server replying with the payload prefixed by the sender address.
async fn udp_echo(addr: SocketAddr) -> SocketAddr {
    let socket = UdpSocket::bind(addr).await.unwrap();
    let local = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65_535];
        loop {
            let (len, peer) = socket.recv_from(&mut buf).await.unwrap();
            let mut reply = format!("{peer}|").into_bytes();
            reply.extend_from_slice(&buf[..len]);
            let _ = socket.send_to(&reply, peer).await;
        }
    });
    local
}

async fn udp_roundtrip(client: &UdpSocket, payload: &[u8]) -> (String, Vec<u8>) {
    client.send(payload).await.unwrap();
    let mut buf = vec![0u8; 65_535];
    let len = timeout(WAIT, client.recv(&mut buf))
        .await
        .expect("udp reply")
        .unwrap();
    let pos = buf[..len].iter().position(|&b| b == b'|').unwrap();
    (
        String::from_utf8(buf[..pos].to_vec()).unwrap(),
        buf[pos + 1..len].to_vec(),
    )
}

/// Finds `count` consecutive ports free for both TCP and UDP on 127.0.0.1.
fn free_port_range(count: u16) -> u16 {
    let seed = (std::process::id() as u16).wrapping_mul(7919);
    'outer: for attempt in 0..200u16 {
        let base = 20_000 + (seed.wrapping_add(attempt.wrapping_mul(97)) % 30_000);
        let mut held = Vec::new();
        for port in base..base + count {
            let tcp = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port));
            let udp = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port));
            match (tcp, udp) {
                (Ok(t), Ok(u)) => held.push((t, u)),
                _ => continue 'outer,
            }
        }
        return base;
    }
    panic!("no free port range");
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_forwards_data_intact() {
    let echo = tcp_echo("127.0.0.1:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("tcp:127.0.0.1:0:127.0.0.1:{}", echo.port()))],
        ForwardConfig::default(),
    );
    let mut client = TcpStream::connect(first(&bindings, Protocol::Tcp))
        .await
        .unwrap();
    let payload: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let (mut read, mut write) = client.split();
    let send = async {
        write.write_all(&payload).await.unwrap();
        write.shutdown().await.unwrap();
    };
    let mut received = Vec::new();
    let recv = read.read_to_end(&mut received);
    let ((), result) = timeout(WAIT, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    result.unwrap();
    assert_eq!(received, payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_half_close_is_propagated() {
    // The target only answers after it sees EOF from the client.
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut data = Vec::new();
        stream.read_to_end(&mut data).await.unwrap();
        stream.write_all(b"got:").await.unwrap();
        stream.write_all(&data).await.unwrap();
    });
    let bindings = start(
        &[spec(&format!(
            "tcp:127.0.0.1:0:127.0.0.1:{}",
            target_addr.port()
        ))],
        ForwardConfig::default(),
    );
    let mut client = TcpStream::connect(first(&bindings, Protocol::Tcp))
        .await
        .unwrap();
    client.write_all(b"hello").await.unwrap();
    client.shutdown().await.unwrap();
    let mut reply = Vec::new();
    timeout(WAIT, client.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, b"got:hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_refused_target_resets_client() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let bindings = start(
        &[spec(&format!("tcp:127.0.0.1:0:127.0.0.1:{port}"))],
        ForwardConfig::default(),
    );
    let mut client = TcpStream::connect(first(&bindings, Protocol::Tcp))
        .await
        .unwrap();
    let mut buf = [0u8; 16];
    let result = timeout(WAIT, client.read(&mut buf)).await.unwrap();
    assert_eq!(result.unwrap_err().kind(), ErrorKind::ConnectionReset);
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_target_reset_is_propagated() {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        stream.set_zero_linger().unwrap();
        drop(stream);
    });
    let bindings = start(
        &[spec(&format!(
            "tcp:127.0.0.1:0:127.0.0.1:{}",
            target_addr.port()
        ))],
        ForwardConfig::default(),
    );
    let mut client = TcpStream::connect(first(&bindings, Protocol::Tcp))
        .await
        .unwrap();
    client.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 16];
    let result = timeout(WAIT, client.read(&mut buf)).await.unwrap();
    assert_eq!(result.unwrap_err().kind(), ErrorKind::ConnectionReset);
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_connect_timeout_resets_client() {
    // A listener with a full backlog that never accepts makes further
    // connects hang; on Linux the SYN is simply dropped.
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
        .unwrap();
    socket.listen(0).unwrap();
    let blackhole = socket.local_addr().unwrap().as_socket().unwrap();
    let mut fillers = Vec::new();
    for _ in 0..4 {
        fillers.push(std::net::TcpStream::connect_timeout(
            &blackhole,
            Duration::from_millis(200),
        ));
    }
    let config = ForwardConfig {
        tcp_connect_timeout: Duration::from_millis(300),
        ..ForwardConfig::default()
    };
    let bindings = start(
        &[spec(&format!(
            "tcp:127.0.0.1:0:127.0.0.1:{}",
            blackhole.port()
        ))],
        config,
    );
    let mut client = TcpStream::connect(first(&bindings, Protocol::Tcp))
        .await
        .unwrap();
    let mut buf = [0u8; 16];
    let result = timeout(WAIT, client.read(&mut buf)).await.unwrap();
    assert_eq!(result.unwrap_err().kind(), ErrorKind::ConnectionReset);
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_tcp_connection_is_never_timed_out() {
    let echo = tcp_echo("127.0.0.1:0".parse().unwrap()).await;
    // Even with an aggressive UDP timeout, idle TCP connections stay open.
    let config = ForwardConfig {
        udp_idle_timeout: Duration::from_millis(50),
        ..ForwardConfig::default()
    };
    let bindings = start(
        &[spec(&format!("tcp:127.0.0.1:0:127.0.0.1:{}", echo.port()))],
        config,
    );
    let mut client = TcpStream::connect(first(&bindings, Protocol::Tcp))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.write_all(b"still here").await.unwrap();
    let mut buf = [0u8; 10];
    timeout(WAIT, client.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"still here");
}

#[tokio::test(flavor = "multi_thread")]
async fn port_ranges_map_one_to_one() {
    let target = free_port_range(3);
    for port in target..target + 3 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = stream.write_all(port.to_string().as_bytes()).await;
            }
        });
        udp_echo(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await;
    }
    // Picked after the targets are bound, so the ranges cannot overlap.
    let local = free_port_range(3);
    start(
        &[spec(&format!(
            "127.0.0.1:{}-{}:127.0.0.1:{target}",
            local,
            local + 2
        ))],
        ForwardConfig::default(),
    );
    for offset in 0..3 {
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, local + offset))
            .await
            .unwrap();
        let mut reply = String::new();
        timeout(WAIT, client.read_to_string(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply, (target + offset).to_string());

        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        udp.connect((Ipv4Addr::LOCALHOST, local + offset))
            .await
            .unwrap();
        let (_, payload) = udp_roundtrip(&udp, b"x").await;
        assert_eq!(payload, b"x");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_forwards_datagrams() {
    let echo = udp_echo("127.0.0.1:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("udp:127.0.0.1:0:127.0.0.1:{}", echo.port()))],
        ForwardConfig::default(),
    );
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .connect(first(&bindings, Protocol::Udp))
        .await
        .unwrap();
    for payload in [&b"hello"[..], b"", &[7u8; 60_000][..]] {
        let (_, reply) = udp_roundtrip(&client, payload).await;
        assert_eq!(reply, payload);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_clients_get_separate_sessions() {
    let echo = udp_echo("127.0.0.1:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("udp:127.0.0.1:0:127.0.0.1:{}", echo.port()))],
        ForwardConfig::default(),
    );
    let forwarder = first(&bindings, Protocol::Udp);
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    a.connect(forwarder).await.unwrap();
    b.connect(forwarder).await.unwrap();
    let (via_a, reply_a) = udp_roundtrip(&a, b"from a").await;
    let (via_b, reply_b) = udp_roundtrip(&b, b"from b").await;
    assert_eq!(reply_a, b"from a");
    assert_eq!(reply_b, b"from b");
    assert_ne!(via_a, via_b);
    // Session reuse: the same client keeps its upstream address.
    let (again_a, _) = udp_roundtrip(&a, b"again").await;
    assert_eq!(via_a, again_a);
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_sessions_expire_when_idle() {
    let echo = udp_echo("127.0.0.1:0".parse().unwrap()).await;
    let config = ForwardConfig {
        udp_idle_timeout: Duration::from_millis(300),
        ..ForwardConfig::default()
    };
    let bindings = start(
        &[spec(&format!("udp:127.0.0.1:0:127.0.0.1:{}", echo.port()))],
        config,
    );
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .connect(first(&bindings, Protocol::Udp))
        .await
        .unwrap();
    let (first_upstream, _) = udp_roundtrip(&client, b"1").await;
    // Activity within the timeout keeps the session alive.
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (upstream, _) = udp_roundtrip(&client, b"keep").await;
        assert_eq!(upstream, first_upstream);
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let (new_upstream, _) = udp_roundtrip(&client, b"2").await;
    assert_ne!(new_upstream, first_upstream);
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_wildcard_listener_roundtrip() {
    let echo = udp_echo("127.0.0.1:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("udp:*:0:127.0.0.1:{}", echo.port()))],
        ForwardConfig::default(),
    );
    let v4 = bindings.iter().find(|b| b.local.is_ipv4()).unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .connect((Ipv4Addr::LOCALHOST, v4.local.port()))
        .await
        .unwrap();
    for payload in [&b"wild"[..], b"", &[1u8; 30_000][..]] {
        let (_, reply) = udp_roundtrip(&client, payload).await;
        assert_eq!(reply, payload);
    }
}

/// Whether the platform honours `IP_PKTINFO` as a source address on send.
/// Some sandboxes (e.g. gVisor) accept but ignore it.
fn ip_pktinfo_send_supported() -> bool {
    use std::os::fd::AsRawFd;
    let server = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let one: libc::c_int = 1;
    // SAFETY: valid fd and c_int option value.
    unsafe {
        libc::setsockopt(
            server.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_PKTINFO,
            std::ptr::from_ref(&one).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    let port = server.local_addr().unwrap().port();
    let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    client.connect((Ipv4Addr::new(127, 0, 0, 2), port)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    client.send(b"probe").unwrap();
    let mut buf = [0u8; 16];
    let (_, peer) = server.recv_from(&mut buf).unwrap();
    let peer: socket2::SockAddr = peer.into();
    let info = libc::in_pktinfo {
        ipi_ifindex: 0,
        ipi_spec_dst: libc::in_addr {
            s_addr: u32::from(Ipv4Addr::new(127, 0, 0, 2)).to_be(),
        },
        ipi_addr: libc::in_addr { s_addr: 0 },
    };
    let mut control = [0u64; 8];
    let mut iov = libc::iovec {
        iov_base: b"probe".as_ptr().cast_mut().cast(),
        iov_len: 5,
    };
    // SAFETY: all msghdr pointers refer to live buffers of the given sizes.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_name = peer.as_ptr().cast_mut().cast();
        msg.msg_namelen = peer.len();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        let size = size_of::<libc::in_pktinfo>() as u32;
        msg.msg_controllen = libc::CMSG_SPACE(size) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::IPPROTO_IP;
        (*cmsg).cmsg_type = libc::IP_PKTINFO;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), info);
        libc::sendmsg(server.as_raw_fd(), &msg, 0);
    }
    client.recv(&mut buf).is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_wildcard_replies_from_the_address_the_client_used() {
    if !ip_pktinfo_send_supported() {
        eprintln!("skipping: platform ignores IP_PKTINFO source addresses");
        return;
    }
    let echo = udp_echo("127.0.0.1:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("udp:0.0.0.0:0:127.0.0.1:{}", echo.port()))],
        ForwardConfig::default(),
    );
    let port = first(&bindings, Protocol::Udp).port();
    // A connected client only accepts datagrams from the exact address it
    // sent to; without pktinfo the reply would come from 127.0.0.1.
    for dst in [Ipv4Addr::new(127, 0, 0, 2), Ipv4Addr::new(127, 0, 0, 3)] {
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect((dst, port)).await.unwrap();
        let (_, reply) = udp_roundtrip(&client, b"multi-homed").await;
        assert_eq!(reply, b"multi-homed");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_unreachable_target_keeps_session() {
    let closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let bindings = start(
        &[spec(&format!("udp:127.0.0.1:0:127.0.0.1:{port}"))],
        ForwardConfig::default(),
    );
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .connect(first(&bindings, Protocol::Udp))
        .await
        .unwrap();
    client.send(b"lost").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The target comes up on the same port; the existing session works.
    let echo = udp_echo(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await;
    assert_eq!(echo.port(), port);
    let (_, reply) = udp_roundtrip(&client, b"found").await;
    assert_eq!(reply, b"found");
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_listeners_are_ipv6_only() {
    // Binding [::] must not take the IPv4 port as well.
    let v6 = forward::bind_all(&[spec("tcp:[::]:0:127.0.0.1:1")]).unwrap();
    let port = v6[0].local_addr().port();
    let v4 = forward::bind_all(&[spec(&format!("tcp:0.0.0.0:{port}:127.0.0.1:1"))]);
    assert!(v4.is_ok(), "{:?}", v4.err());
}

#[tokio::test(flavor = "multi_thread")]
async fn wildcard_binds_ipv4_and_ipv6() {
    let echo = tcp_echo("127.0.0.1:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("*:0:127.0.0.1:{}", echo.port()))],
        ForwardConfig::default(),
    );
    assert_eq!(bindings.len(), 4);
    assert_eq!(bindings.iter().filter(|b| b.local.is_ipv6()).count(), 2);
    let v4 = bindings
        .iter()
        .find(|b| b.protocol == Protocol::Tcp && b.local.is_ipv4())
        .unwrap();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, v4.local.port()))
        .await
        .unwrap();
    client.write_all(b"v4").await.unwrap();
    let mut buf = [0u8; 2];
    timeout(WAIT, client.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"v4");
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_to_ipv4_and_back() {
    if !ipv6_loopback_available() {
        eprintln!("skipping: no IPv6 loopback in this environment");
        return;
    }
    let echo4 = tcp_echo("127.0.0.1:0".parse().unwrap()).await;
    let echo6 = tcp_echo("[::1]:0".parse().unwrap()).await;
    let bindings = start(
        &[
            spec(&format!("tcp:[::1]:0:127.0.0.1:{}", echo4.port())),
            spec(&format!("tcp:127.0.0.1:0:[::1]:{}", echo6.port())),
        ],
        ForwardConfig::default(),
    );
    for binding in bindings {
        let mut client = TcpStream::connect(binding.local).await.unwrap();
        client.write_all(b"xx").await.unwrap();
        let mut buf = [0u8; 2];
        timeout(WAIT, client.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_udp_wildcard() {
    if !ipv6_loopback_available() {
        eprintln!("skipping: no IPv6 loopback in this environment");
        return;
    }
    let echo = udp_echo("[::1]:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("udp:[::]:0:[::1]:{}", echo.port()))],
        ForwardConfig::default(),
    );
    let client = UdpSocket::bind("[::1]:0").await.unwrap();
    client
        .connect((Ipv6Addr::LOCALHOST, first(&bindings, Protocol::Udp).port()))
        .await
        .unwrap();
    let (_, reply) = udp_roundtrip(&client, b"v6").await;
    assert_eq!(reply, b"v6");
}

#[tokio::test(flavor = "multi_thread")]
async fn bind_failure_releases_everything() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken_port = taken.local_addr().unwrap().port();
    let free = free_port_range(1);
    let err = forward::bind_all(&[
        spec(&format!("tcp:127.0.0.1:{free}:127.0.0.1:1")),
        spec(&format!("tcp:127.0.0.1:{taken_port}:127.0.0.1:1")),
    ])
    .err()
    .expect("bind must fail");
    assert_eq!(err.binding.local.port(), taken_port);
    assert_eq!(err.source.kind(), ErrorKind::AddrInUse);
    // The socket bound before the failure was released again.
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, free)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn many_concurrent_tcp_connections() {
    let echo = tcp_echo("127.0.0.1:0".parse().unwrap()).await;
    let bindings = start(
        &[spec(&format!("tcp:127.0.0.1:0:127.0.0.1:{}", echo.port()))],
        ForwardConfig::default(),
    );
    let addr = first(&bindings, Protocol::Tcp);
    let mut tasks = Vec::new();
    for i in 0..200u32 {
        tasks.push(tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            let msg = i.to_be_bytes();
            client.write_all(&msg).await.unwrap();
            let mut buf = [0u8; 4];
            client.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, msg);
        }));
    }
    for task in tasks {
        timeout(WAIT, task).await.unwrap().unwrap();
    }
}
