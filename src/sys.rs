//! Small OS helpers.

/// Returns true when running with an effective user id of 0.
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Raises the soft open file limit to the hard limit, so that large port
/// ranges and many concurrent connections do not run out of descriptors.
/// Returns the resulting soft limit.
pub fn raise_fd_limit() -> std::io::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable rlimit struct.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if limit.rlim_cur < limit.rlim_max {
        let raised = libc::rlimit {
            rlim_cur: limit.rlim_max,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: `raised` is a valid rlimit struct.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        limit = raised;
    }
    Ok(limit.rlim_cur)
}

/// Parses `ip_local_port_range` content such as `"32768\t60999"`.
pub fn parse_port_range_pair(text: &str) -> Option<(u16, u16)> {
    let mut parts = text.split_whitespace();
    let start = parts.next()?.parse().ok()?;
    let end = parts.next()?.parse().ok()?;
    (parts.next().is_none() && start <= end).then_some((start, end))
}

/// Parses `ip_local_reserved_ports` content such as `"8080,9000-9100"`.
pub fn parse_port_list(text: &str) -> Option<Vec<(u16, u16)>> {
    text.trim()
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (a.parse().ok()?, b.parse().ok()?);
                (a <= b).then_some((a, b))
            }
            None => part.parse().ok().map(|p| (p, p)),
        })
        .collect()
}

/// Returns true if `port` is in the kernel's ephemeral port range and not
/// reserved, i.e. outgoing connections may temporarily hold it.
pub fn is_unreserved_ephemeral_port(port: u16) -> bool {
    let read = |name| std::fs::read_to_string(format!("/proc/sys/net/ipv4/{name}")).ok();
    let Some((start, end)) = read("ip_local_port_range").and_then(|t| parse_port_range_pair(&t))
    else {
        return false;
    };
    let reserved = read("ip_local_reserved_ports")
        .and_then(|t| parse_port_list(&t))
        .unwrap_or_default();
    (start..=end).contains(&port) && !reserved.iter().any(|&(a, b)| (a..=b).contains(&port))
}

/// Whether new IPv6 sockets accept IPv4 traffic by default, i.e. what an
/// IPv6 bind that does not touch `IPV6_V6ONLY` gets. Asks the kernel directly
/// (a fresh socket reflects `net.ipv6.bindv6only`); false without IPv6.
pub fn ipv6_dual_stack_default() -> bool {
    socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, None)
        .and_then(|socket| socket.only_v6())
        .is_ok_and(|only_v6| !only_v6)
}
