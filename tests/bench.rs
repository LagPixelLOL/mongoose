//! TCP copy buffer size benchmark. Ignored by default; run with
//!
//! ```text
//! cargo test --release --test bench -- --ignored --nocapture
//! ```
//!
//! Environment overrides:
//! * `MONGOOSE_BENCH_SIZES_KIB` (default `8,16,32,64`)
//! * `MONGOOSE_BENCH_MIB` megabytes per throughput run (default 128)
//! * `MONGOOSE_BENCH_CONNS` connections for the memory test (default 1000)

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mongoose::forward::{self, ForwardConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

const THROUGHPUT_RUNS: usize = 3;
const PING_SAMPLES: usize = 2000;
const PING_SIZE: usize = 64;

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
        .unwrap()
}

/// Returns freed heap memory to the OS so RSS deltas are not hidden by
/// memory cached from a previous round.
fn trim_heap() {
    #[cfg(target_env = "gnu")]
    // SAFETY: malloc_trim has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Discards everything; replies with one byte after EOF.
async fn sink() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 256 * 1024];
                while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                let _ = stream.write_all(b"k").await;
            });
        }
    });
    addr
}

async fn echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    addr
}

/// Starts a forwarder on its own runtime, like the real binary.
fn forwarder(rt: &Runtime, target: SocketAddr, size: usize) -> (SocketAddr, JoinHandle<()>) {
    let _guard = rt.enter();
    let spec = format!("tcp:127.0.0.1:0:127.0.0.1:{}", target.port())
        .parse()
        .unwrap();
    let listeners = forward::bind_all(&[spec]).unwrap();
    let addr = listeners[0].local_addr();
    let config = ForwardConfig {
        tcp_buffer_size: size,
        ..ForwardConfig::default()
    };
    (addr, rt.spawn(forward::serve(listeners, config)))
}

async fn throughput_gbit(addr: SocketAddr, bytes: usize) -> f64 {
    let chunk = vec![0x5au8; 256 * 1024];
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.set_nodelay(true).unwrap();
    let start = Instant::now();
    let mut sent = 0;
    while sent < bytes {
        stream.write_all(&chunk).await.unwrap();
        sent += chunk.len();
    }
    stream.shutdown().await.unwrap();
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).await.unwrap();
    sent as f64 * 8.0 / start.elapsed().as_secs_f64() / 1e9
}

async fn ping_us(addr: SocketAddr) -> Vec<f64> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.set_nodelay(true).unwrap();
    let msg = [1u8; PING_SIZE];
    let mut buf = [0u8; PING_SIZE];
    let mut samples = Vec::with_capacity(PING_SAMPLES);
    for _ in 0..PING_SAMPLES {
        let start = Instant::now();
        stream.write_all(&msg).await.unwrap();
        stream.read_exact(&mut buf).await.unwrap();
        samples.push(start.elapsed().as_secs_f64() * 1e6);
    }
    samples.sort_by(f64::total_cmp);
    samples
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// RSS added per connection once both copy buffers have been filled.
async fn rss_per_conn_kib(addr: SocketAddr, conns: usize, size: usize) -> f64 {
    let payload: Arc<[u8]> = vec![7u8; 2 * size].into();
    trim_heap();
    let before = rss_kib();
    let mut tasks = Vec::with_capacity(conns);
    for _ in 0..conns {
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let (mut r, mut w) = stream.split();
            let mut back = vec![0u8; payload.len()];
            let (sent, received) = tokio::join!(w.write_all(&payload), r.read_exact(&mut back));
            sent.unwrap();
            received.unwrap();
            stream
        }));
    }
    let mut streams = Vec::with_capacity(conns);
    for task in tasks {
        streams.push(task.await.unwrap());
    }
    let after = rss_kib();
    drop(streams);
    after.saturating_sub(before) as f64 / conns as f64
}

const WIDTHS: [usize; 6] = [6, 23, 19, 21, 8, 20];

fn print_row(cells: &[String; 6]) {
    let cells: Vec<String> = cells
        .iter()
        .zip(WIDTHS)
        .map(|(cell, width)| format!("{cell:>width$}"))
        .collect();
    println!("{}", cells.join(" | "));
}

#[test]
#[ignore = "benchmark: cargo test --release --test bench -- --ignored --nocapture"]
fn tcp_buffer_size() {
    let sizes: Vec<usize> = std::env::var("MONGOOSE_BENCH_SIZES_KIB")
        .unwrap_or_else(|_| "8,16,32,64".into())
        .split(',')
        .map(|s| s.trim().parse::<usize>().expect("size in KiB") * 1024)
        .collect();
    let bytes = env_or("MONGOOSE_BENCH_MIB", 128usize) << 20;
    let conns = env_or("MONGOOSE_BENCH_CONNS", 1000usize);
    let _ = mongoose::sys::raise_fd_limit();

    if cfg!(debug_assertions) {
        eprintln!("warning: debug build; use --release for meaningful numbers");
    }
    let forward_rt = Runtime::new().unwrap();
    let load_rt = Runtime::new().unwrap();
    let (sink_addr, echo_addr) = load_rt.block_on(async { (sink().await, echo().await) });
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let started = Instant::now();

    println!();
    println!(
        "TCP copy buffer benchmark: {} MiB x {THROUGHPUT_RUNS} runs, {PING_SAMPLES} pings, \
         {conns} conns, {threads} CPUs",
        bytes >> 20
    );
    let header = [
        "buffer",
        "Gbit/s median (min-max)",
        "idle RTT p50/p99 us",
        "loaded RTT p50/p99 us",
        "KiB/conn",
        "max queue 100M/1G ms",
    ];
    print_row(&header.map(String::from));
    println!(
        "{}",
        "-".repeat(WIDTHS.iter().sum::<usize>() + 3 * (WIDTHS.len() - 1))
    );

    for &size in &sizes {
        let (to_sink, sink_fwd) = forwarder(&forward_rt, sink_addr, size);
        let (to_echo, echo_fwd) = forwarder(&forward_rt, echo_addr, size);

        let row = load_rt.block_on(async {
            let mut runs = Vec::new();
            for _ in 0..THROUGHPUT_RUNS {
                runs.push(throughput_gbit(to_sink, bytes).await);
            }
            runs.sort_by(f64::total_cmp);

            let idle = ping_us(to_echo).await;

            // Saturate the forwarder with bulk transfers while pinging.
            let stop = Arc::new(AtomicBool::new(false));
            let mut bulk = Vec::new();
            for _ in 0..2 {
                let stop = stop.clone();
                bulk.push(tokio::spawn(async move {
                    let chunk = vec![0u8; 256 * 1024];
                    let mut stream = TcpStream::connect(to_sink).await.unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        stream.write_all(&chunk).await.unwrap();
                    }
                }));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let loaded = ping_us(to_echo).await;
            stop.store(true, Ordering::Relaxed);
            for task in bulk {
                task.await.unwrap();
            }

            let per_conn = rss_per_conn_kib(to_echo, conns, size).await;
            (runs, idle, loaded, per_conn)
        });
        sink_fwd.abort();
        echo_fwd.abort();
        // Let the aborted forwarder close its connections before trimming.
        std::thread::sleep(Duration::from_millis(50));
        trim_heap();

        let (runs, idle, loaded, per_conn) = row;
        let queue_ms = |bits_per_sec: f64| size as f64 * 8.0 / bits_per_sec * 1e3;
        print_row(&[
            format!("{} KiB", size / 1024),
            format!(
                "{:.1} ({:.1}-{:.1})",
                runs[runs.len() / 2],
                runs[0],
                runs[runs.len() - 1]
            ),
            format!(
                "{:.0}/{:.0}",
                percentile(&idle, 0.5),
                percentile(&idle, 0.99)
            ),
            format!(
                "{:.0}/{:.0}",
                percentile(&loaded, 0.5),
                percentile(&loaded, 0.99)
            ),
            format!("{per_conn:.0}"),
            format!("{:.2}/{:.3}", queue_ms(100e6), queue_ms(1e9)),
        ]);
    }
    println!(
        "\nKiB/conn: RSS of the whole test process per open connection (forwarder \
         buffers + load generator); compare rows, not absolute values."
    );
    println!(
        "max queue: worst-case extra delay one full buffer adds on a saturated link \
         of that speed."
    );
    println!("total time: {:.1}s", started.elapsed().as_secs_f64());
}
