use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

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
        stderr.contains("conflicting forwarding rule pair"),
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

    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = child.wait().unwrap();
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
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    child.wait().unwrap();
    assert!(saw_debug);
}

#[test]
fn bind_failure_exits_with_error() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let output = mongoose(&["start", &format!("tcp:127.0.0.1:{port}:127.0.0.1:1")]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to bind tcp 127.0.0.1:"), "{stderr}");
    assert!(stderr.contains("Address already in use"), "{stderr}");
    // The cause is reported once, not repeated.
    assert_eq!(
        stderr.matches("Address already in use").count(),
        1,
        "{stderr}"
    );
}
