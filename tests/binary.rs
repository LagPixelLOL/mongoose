use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Waits for the child, killing it and failing the test if it takes too long.
fn wait_with_deadline(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("mongoose did not exit in time");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn sigterm(child: &Child) {
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
}

fn mongoose(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mongoose"))
        .args(args)
        .output()
        .unwrap()
}

/// Checks the `[yy/mm/dd hh:mm:ss:mmm+N LEVEL target] message` layout.
fn assert_log_line(line: &str, level: &str) {
    let rest = line.strip_prefix('[').expect(line);
    let (timestamp, rest) = rest.split_at(21);
    let bytes = timestamp.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let expected_sep = match i {
            2 | 5 => Some(b'/'),
            8 => Some(b' '),
            11 | 14 | 17 => Some(b':'),
            _ => None,
        };
        match expected_sep {
            Some(sep) => assert_eq!(b, sep, "{line}"),
            None => assert!(b.is_ascii_digit(), "{line}"),
        }
    }
    let (offset, rest) = rest.split_once(' ').expect(line);
    let sign = offset.as_bytes()[0];
    assert!(sign == b'+' || sign == b'-', "{line}");
    let number = &offset[1..];
    assert!(number.parse::<f64>().is_ok(), "{line}");
    if let Some((_, decimals)) = number.split_once('.') {
        assert!(decimals.len() <= 2, "{line}");
    }
    assert!(rest.starts_with(level), "{line}");
}

#[test]
fn conflicting_rules_fail_before_binding() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let rule = format!("tcp:127.0.0.1:{port}:127.0.0.1:1");
    let output = mongoose(&["start", &rule, &format!("tcp:*:{port}:127.0.0.1:2")]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("conflict(s) between forwarding rules"),
        "{stderr}"
    );
    assert!(!stderr.contains("rule #1: "), "{stderr}");
}

#[test]
fn usage_errors_exit_with_2() {
    for args in [
        &["-v", "start", "-v", "tcp:*:1:1.1.1.1:1"][..],
        &["start", "tcp:*:1:*:1"],
        &["start"],
        &["bogus"],
    ] {
        let output = mongoose(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn show_works_without_systemd() {
    let output = mongoose(&["show"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Unit file:"), "{stdout}");
    assert!(
        stdout.contains("journalctl -u mongoose.service"),
        "{stdout}"
    );
}

#[test]
fn start_logs_rules_forwards_and_stops_on_sigterm() {
    let echo = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in echo.incoming() {
            let mut stream = stream.unwrap();
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).unwrap();
            stream.write_all(&buf).unwrap();
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_mongoose"))
        .args(["start", &format!("tcp:127.0.0.1:0:127.0.0.1:{echo_port}")])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();

    let rule = lines.next().unwrap().unwrap();
    assert_log_line(&rule, "INFO ");
    assert!(
        rule.ends_with(&format!(
            "rule #1: tcp 127.0.0.1:0 -> 127.0.0.1:{echo_port} (ephemeral local port, 1 socket)"
        )),
        "{rule}"
    );
    let bound = lines.next().unwrap().unwrap();
    assert!(bound.contains("(ephemeral port)"), "{bound}");
    let addr = bound
        .split("listening on tcp ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .to_string();
    let ready = lines.next().unwrap().unwrap();
    assert!(ready.ends_with("forwarding on 1 socket(s)"), "{ready}");

    let mut client = TcpStream::connect(&addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    client.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");

    sigterm(&child);
    let status = wait_with_deadline(&mut child);
    assert!(status.success(), "{status}");
    let rest: Vec<String> = lines.map(Result::unwrap).collect();
    assert!(
        rest.iter().any(|l| l.ends_with("shutting down")),
        "{rest:?}"
    );
}

#[test]
fn verbose_enables_debug_logs() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mongoose"))
        .args(["start", "-v", "tcp:127.0.0.1:0:127.0.0.1:9"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut saw_debug = false;
    for line in lines.by_ref() {
        let line = line.unwrap();
        if line.contains(" DEBUG ") {
            assert_log_line(&line, "DEBUG");
            saw_debug = true;
        }
        if line.contains("forwarding on") {
            break;
        }
    }
    sigterm(&child);
    assert!(wait_with_deadline(&mut child).success());
    assert!(saw_debug);
}

#[test]
fn port_in_use_warns_and_keeps_running() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mongoose"))
        .args(["start", &format!("tcp:127.0.0.1:{port}:127.0.0.1:1")])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let rule = lines.next().unwrap().unwrap();
    assert!(rule.contains("rule #1: "), "{rule}");
    let warning = lines.next().unwrap().unwrap();
    assert_log_line(&warning, "WARN ");
    assert!(
        warning.ends_with(&format!(
            "port already in use, retrying every 60s: tcp 127.0.0.1:{port} -> 127.0.0.1:1"
        )),
        "{warning}"
    );
    let mut ready = lines.next().unwrap().unwrap();
    if ready.contains("ephemeral port range") {
        assert_log_line(&ready, "WARN ");
        ready = lines.next().unwrap().unwrap();
    }
    assert!(
        ready.ends_with("forwarding on 0 socket(s), 1 waiting for their port"),
        "{ready}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(child.try_wait().unwrap().is_none(), "must keep running");
    sigterm(&child);
    assert!(wait_with_deadline(&mut child).success());
}

#[test]
fn other_bind_errors_exit_with_error() {
    // 192.0.2.1 (TEST-NET-1) is not a local address.
    let output = mongoose(&["start", "tcp:192.0.2.1:4000:127.0.0.1:1"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to bind tcp 192.0.2.1:4000"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Cannot assign requested address"),
        "{stderr}"
    );
    // The cause is reported once, not repeated.
    assert_eq!(
        stderr.matches("Cannot assign requested address").count(),
        1,
        "{stderr}"
    );
}

#[test]
fn sigterm_right_after_startup_is_not_lost() {
    for _ in 0..10 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mongoose"))
            .args(["start", "tcp:127.0.0.1:0:127.0.0.1:9"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stderr.take().unwrap()).lines();
        for line in lines {
            if line.unwrap().contains("forwarding on") {
                break;
            }
        }
        sigterm(&child);
        assert!(wait_with_deadline(&mut child).success());
    }
}
