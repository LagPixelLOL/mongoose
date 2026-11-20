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

/// Properties of the filesystem a path lives on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Storage {
    /// tmpfs/ramfs: contents are lost on reboot.
    pub temporary: bool,
    pub read_only: bool,
}

const RAMFS_MAGIC: i64 = 0x8584_58f6;
const FS_IMMUTABLE_FL: libc::c_long = 0x10;
const FS_APPEND_FL: libc::c_long = 0x20;

fn path_cstring(path: &std::path::Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// Filesystem properties of `path` (following symlinks).
pub fn storage(path: &std::path::Path) -> std::io::Result<Storage> {
    let c_path = path_cstring(path)?;
    // SAFETY: zeroed statfs/statvfs are valid out-parameters; c_path is a
    // NUL-terminated string.
    unsafe {
        let mut fs: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &mut fs) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut vfs: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut vfs) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The integer types of f_type and the magic numbers vary by arch.
        #[allow(clippy::unnecessary_cast)]
        let (fs_type, tmpfs) = (fs.f_type as i64, libc::TMPFS_MAGIC as i64);
        Ok(Storage {
            temporary: fs_type == tmpfs || fs_type == RAMFS_MAGIC,
            read_only: vfs.f_flag & libc::ST_RDONLY != 0,
        })
    }
}

/// Whether `path` (not following symlinks) has the immutable or append-only
/// attribute (`chattr +i` / `+a`), which prevents deleting or replacing it
/// even as root. Filesystems without attribute support report false.
pub fn is_immutable(path: &std::path::Path) -> std::io::Result<bool> {
    let c_path = path_cstring(path)?;
    // SAFETY: open/ioctl/close on a descriptor we own; flags is a valid
    // out-parameter for FS_IOC_GETFLAGS.
    unsafe {
        let fd = libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut flags: libc::c_long = 0;
        let rc = libc::ioctl(fd, libc::FS_IOC_GETFLAGS, &mut flags);
        let err = std::io::Error::last_os_error();
        libc::close(fd);
        if rc != 0 {
            return match err.raw_os_error() {
                Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS) => Ok(false),
                _ => Err(err),
            };
        }
        Ok(flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0)
    }
}

/// Mount points listed in `/proc/self/mountinfo` content (field 5, with the
/// kernel's octal escapes for space, tab, newline and backslash undone).
pub fn parse_mount_points(mountinfo: &str) -> Vec<std::path::PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| {
            std::path::PathBuf::from(
                field
                    .replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\"),
            )
        })
        .collect()
}

/// Whether `path` itself is a mount point (e.g. a bind-mounted unit file),
/// which can be neither replaced nor deleted.
pub fn is_mount_point(path: &std::path::Path) -> std::io::Result<bool> {
    let path = std::fs::canonicalize(path)?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(parse_mount_points(&mountinfo).contains(&path))
}
