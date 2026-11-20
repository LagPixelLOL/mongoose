# Mongoose

Mongoose is a simple TCP and UDP port forwarder for Linux. It listens on local
ports and forwards every connection or datagram to a target address, and can
install itself as a systemd service.

```sh
# Forward TCP and UDP port 8080 on all addresses to 10.0.0.5:80
mongoose start '*:8080:10.0.0.5:80'

# Run that as a systemd service that starts at boot
sudo mongoose setup '*:8080:10.0.0.5:80'
```

Mongoose runs on Linux only. For building from source, the project layout and
testing, see [DEVELOPMENT.md](DEVELOPMENT.md).

## Forwarding rules

Every command that forwards takes one or more rules:

```
[tcp:|udp:]LOCAL_IP:LOCAL_PORT:TARGET_IP:TARGET_PORT
```

| Part | Meaning |
| --- | --- |
| `tcp:` / `udp:` | Forward only that protocol. Without a prefix, both TCP and UDP are forwarded. |
| `LOCAL_IP` | Address to listen on. `*` listens on both `0.0.0.0` and `[::]`. |
| `LOCAL_PORT` | A port, or an inclusive range such as `10000-10099`. |
| `TARGET_IP` | Address to forward to. `*` is not allowed here. |
| `TARGET_PORT` | A port, or a range of the same length as the local range. |

- **IPv6 addresses** must be written in brackets, e.g. `[::1]` or
  `[2001:db8::5]`. Forwarding between IPv4 and IPv6 in either direction is
  allowed.
- **Ranges are inclusive**: `10000-10099` is 100 ports. The target range must
  have the same number of ports.
- **A single target port with a local range** means a range of the same
  length starting at that port: `*:10000-10099:10.0.0.5:20000` forwards 10000
  to 20000, 10001 to 20001, and so on up to 10099 to 20099. It never means
  forwarding everything to one port.
- **Port 0** asks the kernel for a free port. It is only allowed as a single
  local port, never in a range or as a target. The port actually used is
  logged at startup.

### Examples

```sh
mongoose start 'tcp:127.0.0.1:2222:10.0.0.5:22'      # TCP only, loopback only
mongoose start 'udp:*:53:[2001:db8::53]:53'          # UDP, all addresses, to IPv6
mongoose start '*:10000-10099:10.0.0.5:20000'        # a port range
mongoose start 'tcp:[::]:443:10.0.0.5:443' 'udp:0.0.0.0:51820:10.0.0.6:51820'
```

Quote rules in the shell so `*` and `[...]` are not expanded.

### IPv4, IPv6 and `*`

- `*` creates two sockets per port: `0.0.0.0` (IPv4 only) and `[::]`
  (IPv6 only).
- An explicit `[::]` creates a single IPv6 socket and keeps your system's
  default for IPv4 traffic (`net.ipv6.bindv6only`). On most systems that
  default is dual-stack, so `[::]` also accepts IPv4 connections. The rule
  summary printed at startup says which applies.
- `0.0.0.0` listens on IPv4 only; a specific address listens on just that
  address.

### Rules are checked strictly

All rules are validated before anything is bound or written, for `start` and
`setup` alike. Mongoose refuses to run, listing every problem, if:

- a rule is malformed. This includes IPv6 addresses without brackets,
  IPv4-mapped IPv6 addresses such as `[::ffff:1.2.3.4]`, uppercase protocol
  prefixes, ports with leading zeros, and ranges that are reversed or would go
  past 65535;
- a target is unusable: `0.0.0.0`, `[::]`, broadcast or multicast;
- two rules collide, i.e. would listen on the same protocol and port with
  overlapping addresses. For example, `*:80` collides with `127.0.0.1:80`, and
  a dual-stack `[::]:80` collides with `0.0.0.0:80`. There is no precedence
  between rules.

Right after these checks pass, Mongoose logs one line per rule showing exactly
what it will do:

```
rule #1: tcp+udp 0.0.0.0:10000-10099, [::]:10000-10099 -> 10.0.0.5:20000-20099 (100 ports, 400 sockets)
```

## Commands

Every command accepts `-v`/`--verbose`, at most once, either before or after
the command name.

### `mongoose start RULE...`

Forwards in the foreground until it receives Ctrl-C or SIGTERM.

- **Port already in use:** a warning is logged and Mongoose keeps running,
  retrying every minute. Each failed retry logs a warning and success logs an
  info line. Ports in the kernel's ephemeral range (usually 32768–60999) can
  be held briefly by outgoing connections; the warning then suggests
  reserving them with `net.ipv4.ip_local_reserved_ports`.
- **Any other bind error** (for example, the address is not on this machine,
  or permission is denied) stops Mongoose with an error.
- **A listening socket lost while running** (for example, killed with `ss -K`)
  also stops Mongoose with an error. Under systemd the service is restarted
  three seconds later.

### `mongoose setup [-s] [-f] RULE...`

Installs Mongoose as the systemd service `mongoose.service`. Requires root and
a system running systemd.

1. Checks the rules, root, systemd and the existing unit (see below).
2. Writes the unit file.
3. Reloads systemd, enables the unit, and restarts it, so a service that is
   already running picks up the new rules.

The unit runs the `mongoose` binary from where it is now. It is never copied
or moved, so keep it in a permanent location such as `/usr/local/bin`. The
unit only contains settings that differ from systemd's defaults:

```ini
# Generated by `mongoose setup`. Managed automatically; do not edit.
[Unit]
Description=Mongoose port forwarder
Wants=network-online.target
After=network-online.target

[Service]
ExecStart=/usr/local/bin/mongoose start *:8080:10.0.0.5:80
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

The first line marks the unit as written by `setup`.

- **Where the unit goes:** if a `mongoose.service` already exists anywhere in
  systemd's search path, `setup` replaces that file in place. Otherwise it
  writes `/etc/systemd/system/mongoose.service`.
- **Units not written by `setup`** are only replaced with `-f`/`--force`.
- **`-s`/`--skip-enable`** only writes the unit file. It does not reload
  systemd, and does not enable, start or restart the unit. A service that is
  already running keeps its previous rules; a warning shows how to apply the
  new ones.
- **On failure:** if a `systemctl` step fails after the unit file was written,
  the error is reported and the file stays as written. Nothing is rolled back.

### `mongoose remove [-f]`

Stops and disables the service, deletes its unit file and reloads systemd.
Requires root. Without `-f`, it only removes a unit written by `setup`.
Drop-in directories (`mongoose.service.d`) are left in place, with a warning.

### `mongoose show`

Prints the service's unit file, its rules and its current state, and points
to `systemctl status` and `journalctl` for logs. Does not require root.

```
Unit file:    /etc/systemd/system/mongoose.service
Managed:      yes (written by `mongoose setup`)
Executable:   /usr/local/bin/mongoose
Rules:
  #1 *:8080:10.0.0.5:80
     tcp+udp 0.0.0.0:8080, [::]:8080 -> 10.0.0.5:80 (1 port, 4 sockets)
Loaded:       loaded
Active:       active (running) since Fri 2026-11-20 03:20:53 UTC
Enabled:      enabled
Main PID:     5051
Restarts:     0
Logs:         see `systemctl status mongoose.service` or `journalctl -u mongoose.service`
```

`show` also reads units not written by `setup`, on a best-effort basis. It
understands the usual systemd syntax: comments, line continuations,
`ExecStart` prefixes, quoting and escapes. Anything only systemd can resolve,
like `%h` or `$VAR`, is reported rather than guessed. `show` also warns about
rules that collide, conflicting unit files and drop-ins, and changes systemd
has not reloaded yet.

### What `--force` will touch

`-f` bypasses only the "written by `setup`" check, and only for a unit file
that is clearly removable:

- a regular file: not a symlink (masked units and package links are
  symlinks), directory or other special file;
- exactly one hard link, under 1 MiB, valid UTF-8 text;
- not on temporary storage (tmpfs, e.g. units in `/run`);
- deletable: not on a read-only filesystem, not mounted over, and not
  immutable or append-only (`chattr +i`/`+a`), and the same goes for its
  directory.

`--force` never overrides the other checks:

- root and systemd are required;
- only one `mongoose.service` may exist in systemd's search path;
- no `mongoose.service.d` drop-ins may exist (`setup` only);
- rules must not collide.

Even without `--force`, `setup` refuses to write a unit to temporary storage
(it would disappear at reboot), and `remove` checks that the file can be
deleted before stopping anything.

## Runtime behaviour

| Aspect | Behaviour |
| --- | --- |
| TCP keepalive | First probe after 3 minutes idle, then every 30 s; the connection is dropped after 3 unanswered probes. Applied to both sides of every connection. |
| TCP idle timeout | None. A connection is only closed by its endpoints, by errors, or by failed keepalives. A quiet connection that answers keepalives is never closed. |
| TCP connect timeout | 10 s. If the target refuses, times out or resets, the client is reset too. |
| TCP half-close | Passed through: one side finishing sending does not end the other direction. |
| UDP sessions | One upstream socket per client address. A session ends after 10 minutes without traffic in either direction. |
| UDP replies | On `0.0.0.0`, `[::]` and `*`, replies come from the exact address the client sent to, so multi-address hosts work. |
| Buffers | 16 KiB per direction per TCP connection, deliberately small to keep memory use and latency low on busy links. |
| File descriptors | The open file limit is raised to the system's hard limit at startup. |

## Logging

Logs go to stderr, coloured when it is a terminal:

```
[26/11/20 03:21:37:922+0 WARN  mongoose::forward] port already in use, retrying every 60s: tcp 0.0.0.0:47022 -> 127.0.0.1:22
```

Timestamps are `yy/mm/dd hh:mm:ss:mmm` followed by the local UTC offset in
hours, e.g. `+0`, `+2`, `-3.5`, `+5.75`. Unusual offsets are rounded to two
decimals. Info level is the default. `-v` adds debug output, such as
individual connections and UDP sessions.

## Exit status

| Code | Meaning |
| --- | --- |
| 0 | Success, or `start` was stopped by a signal |
| 1 | An error, e.g. colliding rules, a bind failure, a lost socket, or refusing to touch a unit |
| 2 | Invalid command line, including malformed rules |
