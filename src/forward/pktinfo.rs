//! `IP_PKTINFO` / `IPV6_PKTINFO` support for UDP sockets bound to a wildcard
//! address.
//!
//! On a multi-homed host (or an IPv6 host with several addresses on one
//! interface) a reply sent from a socket bound to `0.0.0.0`/`[::]` gets its
//! source address picked by the routing table, which may differ from the
//! address the client sent to; most clients drop such replies. We record the
//! destination address of every received datagram and send replies from it.

use std::io;
use std::mem;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use std::ptr;

use socket2::{SockAddr, SockAddrStorage, Socket};

/// The local address a datagram was received on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LocalDst {
    pub ip: IpAddr,
    pub ifindex: u32,
}

/// Room for one `in6_pktinfo` (the larger one) control message; u64 for
/// `cmsghdr` alignment.
type ControlBuf = [u64; 8];

fn set_flag(fd: RawFd, level: libc::c_int, name: libc::c_int) -> io::Result<()> {
    let one: libc::c_int = 1;
    // SAFETY: valid fd and a pointer/length pair for a c_int.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            ptr::from_ref(&one).cast(),
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Asks the kernel to report the destination address of received datagrams.
pub(super) fn enable(socket: &Socket, ipv6: bool) -> io::Result<()> {
    if ipv6 {
        set_flag(
            socket.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVPKTINFO,
        )
    } else {
        set_flag(socket.as_raw_fd(), libc::IPPROTO_IP, libc::IP_PKTINFO)
    }
}

/// Non-blocking `recvmsg`; returns length, sender and (if packet info is
/// enabled) the local destination address.
pub(super) fn recv_from(
    fd: RawFd,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, Option<LocalDst>)> {
    let mut addr = SockAddrStorage::zeroed();
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control: ControlBuf = [0; 8];
    // SAFETY: msghdr is plain data; all-zero is a valid initial state.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_namelen = addr.size_of();
    msg.msg_name = ptr::from_mut(&mut addr).cast();
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = mem::size_of::<ControlBuf>() as _;

    // SAFETY: every pointer in msg refers to live, correctly sized buffers.
    let len = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT) };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel initialised msg_namelen bytes of addr.
    let peer = unsafe { SockAddr::new(addr, msg.msg_namelen) }
        .as_socket()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-IP sender address"))?;

    let mut local = None;
    // SAFETY: standard cmsg traversal over the buffer described by msg.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let hdr = &*cmsg;
            if hdr.cmsg_level == libc::IPPROTO_IP && hdr.cmsg_type == libc::IP_PKTINFO {
                let info: libc::in_pktinfo = ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast());
                // ipi_spec_dst is the address the kernel would answer from:
                // the destination for unicast, the interface address for
                // broadcasts.
                local = Some(LocalDst {
                    ip: IpAddr::V4(Ipv4Addr::from(u32::from_be(info.ipi_spec_dst.s_addr))),
                    ifindex: info.ipi_ifindex as u32,
                });
            } else if hdr.cmsg_level == libc::IPPROTO_IPV6 && hdr.cmsg_type == libc::IPV6_PKTINFO {
                let info: libc::in6_pktinfo = ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast());
                let ip = Ipv6Addr::from(info.ipi6_addr.s6_addr);
                // Never answer from a multicast address.
                if !ip.is_multicast() {
                    local = Some(LocalDst {
                        ip: IpAddr::V6(ip),
                        ifindex: info.ipi6_ifindex,
                    });
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((len as usize, peer, local))
}

/// Non-blocking `sendmsg`, using `source` as the source address if given.
pub(super) fn send_to(
    fd: RawFd,
    buf: &[u8],
    peer: SocketAddr,
    source: Option<LocalDst>,
) -> io::Result<usize> {
    let addr = SockAddr::from(peer);
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr().cast_mut().cast(),
        iov_len: buf.len(),
    };
    let mut control: ControlBuf = [0; 8];
    // SAFETY: msghdr is plain data; all-zero is a valid initial state.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_name = addr.as_ptr().cast_mut().cast();
    msg.msg_namelen = addr.len();
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;

    if let Some(source) = source {
        msg.msg_control = control.as_mut_ptr().cast();
        // SAFETY: the control buffer is large and aligned enough for one
        // pktinfo message; CMSG_FIRSTHDR is non-null once controllen is set.
        unsafe {
            match source.ip {
                IpAddr::V4(ip) => {
                    let size = mem::size_of::<libc::in_pktinfo>() as u32;
                    msg.msg_controllen = libc::CMSG_SPACE(size) as _;
                    let cmsg = libc::CMSG_FIRSTHDR(&msg);
                    (*cmsg).cmsg_level = libc::IPPROTO_IP;
                    (*cmsg).cmsg_type = libc::IP_PKTINFO;
                    (*cmsg).cmsg_len = libc::CMSG_LEN(size) as _;
                    let info = libc::in_pktinfo {
                        // 0: let routing pick the interface for this source.
                        ipi_ifindex: 0,
                        ipi_spec_dst: libc::in_addr {
                            s_addr: u32::from(ip).to_be(),
                        },
                        ipi_addr: libc::in_addr { s_addr: 0 },
                    };
                    ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), info);
                }
                IpAddr::V6(ip) => {
                    let size = mem::size_of::<libc::in6_pktinfo>() as u32;
                    msg.msg_controllen = libc::CMSG_SPACE(size) as _;
                    let cmsg = libc::CMSG_FIRSTHDR(&msg);
                    (*cmsg).cmsg_level = libc::IPPROTO_IPV6;
                    (*cmsg).cmsg_type = libc::IPV6_PKTINFO;
                    (*cmsg).cmsg_len = libc::CMSG_LEN(size) as _;
                    let info = libc::in6_pktinfo {
                        ipi6_addr: libc::in6_addr {
                            s6_addr: ip.octets(),
                        },
                        // Link-local addresses are only unique per interface.
                        ipi6_ifindex: if ip.is_unicast_link_local() {
                            source.ifindex
                        } else {
                            0
                        },
                    };
                    ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), info);
                }
            }
        }
    }

    // SAFETY: every pointer in msg refers to live, correctly sized buffers.
    let len = unsafe { libc::sendmsg(fd, &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
    if len < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(len as usize)
    }
}
