# Developing Mongoose

## Toolchain and building

Mongoose uses the latest stable Rust; `rust-toolchain.toml` pins the `stable`
channel, so `rustup` picks it up automatically. Dependencies are kept at their
latest releases.

```sh
cargo build             # debug build: target/debug/mongoose
cargo build --release   # release build: target/release/mongoose
```

The release profile enables LTO, a single codegen unit and symbol stripping.

The crate only builds on Linux. On other targets `src/lib.rs` stops the build
with a single `compile_error!`, because the code relies on Linux specifics:
systemd, `IP_PKTINFO`, `/proc`, `statfs`, inode flags and Linux socket
behaviour.

## Project structure

The binary is a thin wrapper around a library: `src/main.rs` only parses
arguments, sets up logging, calls into the library and maps errors to exit
codes. Everything else lives in the library, so it can be tested from
`tests/`. Tests never go into `src/`.

```
src/
  main.rs          binary entry point (Tokio's default multi-threaded runtime)
  lib.rs           module list and the Linux-only gate
  cli.rs           command line definition
  spec.rs          rule parsing, expansion into bindings, rule summaries
  conflict.rs      collision detection between rules
  forward/
    mod.rs         binding, ForwardConfig, serve loop, retry of ports in use
    tcp.rs         TCP forwarding
    udp.rs         UDP forwarding and sessions
    pktinfo.rs     raw recvmsg/sendmsg for UDP reply source addresses
  service.rs       systemd unit management and `show`
  unitfile.rs      best-effort parsing of units not written by `setup`
  logging.rs       logger and timestamp format
  sys.rs           small OS helpers (libc calls, /proc parsing)
tests/
  <area>.rs        one integration test file per area, e.g. spec.rs, forward.rs
  bench.rs         ignored benchmark
```

| Module | Responsibility |
| --- | --- |
| `cli` | clap definitions. `-v` is declared per level rather than `global = true`, because clap silently merges repeated global flags; `Cli::try_parse_args` rejects it if given twice. |
| `spec` | `ForwardSpec` parsing and validation, expansion into concrete `Binding`s, `describe()` for the per-rule log line. |
| `conflict` | Collisions between rules by pairwise port range intersection, aware of the system's IPv6 dual-stack default. |
| `forward` | All socket work. `forward/mod.rs` is the public API (`bind_all`, `serve`, `ForwardConfig`); the submodules are private. |
| `service` | Everything systemd: rendering and strictly decoding our own unit, search path handling, `--force` qualification, `setup`/`remove`/`show`. Host access goes through the `System` trait (`HostSystem` in production, a fake in tests). |
| `unitfile` | Reads arbitrary unit files safely and interprets their `ExecStart=` with the real CLI parser. |
| `logging` | env_logger with its default colours and the `yy/mm/dd hh:mm:ss:mmm+N` timestamp. |
| `sys` | Thin, documented wrappers around libc and `/proc`; pure parsers are separate public functions so they can be tested. |

Where new code goes:

- **Anything that can be tested goes into the library,** with a public API
  small enough to test from `tests/`.
- **Side effects go behind an interface the tests can replace.** The `System`
  trait and `ForwardConfig` exist so timeouts, systemd and filesystem
  properties can be controlled in tests.
- **Parsing is separated from I/O.** Examples are `parse_mount_points`,
  `parse_port_list`, `UnitState::parse` and `parse_foreign_unit`.

## Code style

The overall spirit: strict, explicit, unsurprising and fast.

**Strictness**

- Validate every input completely before any side effect, and report every
  problem rather than the first or an implicit "winner". Colliding rules,
  several unit files and drop-ins are errors; there is no precedence.
- Do not guess. If something can only be resolved elsewhere (systemd
  specifiers, environment variables), say so instead of approximating.
- Respect system defaults unless they must be overridden, and document why
  when they are (e.g. `IPV6_V6ONLY` only for the `[::]` half of `*`).
- Do not act behind the user's back. A command does what it says and nothing
  more: for example, `setup -s` only writes the unit file, even if the
  service is running. Point out what the user may want to do next instead.
- Check before acting, and never roll back. Do every check possible before
  the first write or external command. If a later step fails, report it and
  stop.
- Fail as a whole: losing a listener ends the process, and systemd's
  `Restart=always` brings it back. The only tolerated failure is a port in
  use, which is retried, visibly.

**Errors**

- The library uses `thiserror` enums with specific variants that tests can
  match on. The binary uses `anyhow` and prints the full chain once.
- Messages are lowercase, name the thing concerned (path, rule, binding) and
  say what to do where it is not obvious, e.g. "use --force".
- A cause is reported once: either in the message or as `#[source]`, never
  both.
- `unwrap`/`expect` only for real invariants, such as a poisoned mutex or a
  range known to be ordered. Never for input or I/O.

**Logging**

| Level | Used for |
| --- | --- |
| `error!` | Only in `main`, for the error that ends the program. |
| `warn!` | Degraded but running: port in use, things left in place, `--force` in effect, a running service keeping old rules. |
| `info!` | What the user should see once: one line per rule, startup summary, retry success, setup/remove steps. Never per connection or per port of a range. |
| `debug!` | Per connection, per session, per socket. |

Aggregate when there can be many: merge ports into ranges rather than logging
one line each.

**Performance**

- No per-packet allocation on the fast path, small fixed buffers (16 KiB per
  TCP direction), and no locks held across `.await`.
- Prefer one shared resource per worker thread over one per socket when
  socket counts can be large (port ranges): one 64 KiB UDP buffer per worker
  thread, non-blocking sends first, and copy only when a send would block.
- Measure before changing hot-path parameters; `tests/bench.rs` exists for
  that.

**Code**

- `cargo fmt` formatting and `cargo clippy --all-targets` without warnings.
  Allow a lint only locally and with a reason.
- Comments are for what the code cannot say: why something is done, kernel
  or systemd quirks, invariants. Do not narrate the code. Public items get
  `///` doc comments.
- `unsafe` only for syscalls without a safe wrapper, kept in small functions,
  each block with a `// SAFETY:` comment.
- Keep changes consistent with the surrounding code: naming, module
  boundaries and existing helpers (e.g. `describe_bindings`,
  `read_unit_text`).

**Tests**

- Every behaviour gets a test in `tests/<area>.rs` with a name that states
  the behaviour, e.g. `force_never_touches_temporary_storage`.
- Edge cases are part of the work, not an extra: malformed input, boundaries
  (port 65535, empty values), races, unusual files.
- Tests must be fast and deterministic. Use short timeouts with margins,
  poll for conditions instead of sleeping where possible, and avoid ports
  that parallel tests might grab (the helpers pick ports below the ephemeral
  range).
- When a test depends on platform support, probe for it and skip with a
  `skipping: ...` message; never weaken the assertion instead.

**Commits**

- Small, focused commits with a message saying what changed and why, e.g.
  "Retry in-use ports every minute; exit when a bound listener is lost".

## Tests

```sh
cargo test                  # everything, ~1–2 s after building
cargo test --test forward   # one suite
cargo clippy --all-targets  # should be warning-free
cargo fmt
```

| Suite | Covers |
| --- | --- |
| `tests/spec.rs` | Rule syntax, ranges, address checks, display round trips, summaries |
| `tests/conflict.rs` | Collisions, including dual-stack and wildcard cases |
| `tests/cli.rs` | Commands, flags, verbose-once rule |
| `tests/logging.rs` | Timestamp and UTC offset formatting |
| `tests/forward.rs` | Real loopback sockets: TCP/UDP forwarding, half-close, resets, timeouts, ranges, UDP sessions and expiry, reply source addresses, retry of ports in use (with captured logs), lost listeners, shutdown |
| `tests/service.rs` | Unit management against a fake `System` and temporary directories: setup, replace, remove, `--force` rules, search path, temporary/read-only/immutable storage, `show` |
| `tests/unitfile.rs` | Edge cases of the best-effort unit parser |
| `tests/sys.rs` | Port list and mountinfo parsing, fd limit |
| `tests/binary.rs` | The real `mongoose` binary: exit codes, log format, SIGTERM handling, ports in use, closed stdout |

Timing-based tests use short timeouts with generous margins and wait on
conditions rather than fixed sleeps where possible. They must stay fast:
shorten waits only when the assertion and its margin stay the same.

### Tests that depend on the environment

Some tests check what the platform supports and skip themselves, printing
`skipping: ...` (visible with `-- --nocapture`), when it is missing. This is
the case in sandboxes such as gVisor or containers without IPv6:

- IPv6 tests need `::1`.
- The test that UDP replies come from the right address needs the kernel to
  honour `IP_PKTINFO` source addresses.
- The lost-listener tests need the kernel to wake waiters with a hang-up on
  socket shutdown.
- The dual-stack test needs `net.ipv6.bindv6only = 0`.
- The tmpfs test needs `/dev/shm` to be a tmpfs.

Run the suite on a real Linux machine before relying on those areas. systemd
itself is never called by the tests: `tests/service.rs` uses a fake `System`.
To check the systemd integration, try `setup`, `show` and `remove` on a
disposable machine with systemd.

### Benchmark

`tests/bench.rs` is an ignored benchmark comparing TCP buffer sizes:
throughput, round-trip latency idle and under load, and memory per
connection.

```sh
cargo test --release --test bench -- --ignored --nocapture
MONGOOSE_BENCH_SIZES_KIB=16,32 MONGOOSE_BENCH_MIB=64 MONGOOSE_BENCH_CONNS=500 \
    cargo test --release --test bench -- --ignored --nocapture
```

It takes about 10 seconds on a multi-core machine. On a small VM it can take
over a minute; lower the environment variables there. Results from sandboxes
with expensive system calls (e.g. gVisor) favour larger buffers more than a
normal kernel does.
