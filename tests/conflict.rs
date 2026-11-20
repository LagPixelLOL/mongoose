use mongoose::conflict::check;
use mongoose::spec::{ForwardSpec, PortRange, Protocol};

fn specs(inputs: &[&str]) -> Vec<ForwardSpec> {
    inputs.iter().map(|s| s.parse().unwrap()).collect()
}

#[test]
fn disjoint_specs_pass() {
    check(&specs(&[
        "*:80:10.0.0.1:80",
        "*:81:10.0.0.1:80",
        "tcp:127.0.0.1:53:10.0.0.1:53",
        "udp:127.0.0.1:53:10.0.0.1:53",
        "tcp:127.0.0.1:9000:10.0.0.1:1",
        "tcp:127.0.0.2:9000:10.0.0.1:1",
    ]))
    .unwrap();
}

#[test]
fn ipv4_and_ipv6_wildcards_do_not_collide() {
    check(&specs(&["0.0.0.0:80:10.0.0.1:80", "[::]:80:10.0.0.1:80"])).unwrap();
}

#[test]
fn ephemeral_ports_never_collide() {
    check(&specs(&[
        "tcp:127.0.0.1:0:10.0.0.1:80",
        "tcp:127.0.0.1:0:10.0.0.1:80",
    ]))
    .unwrap();
}

#[test]
fn identical_specs_collide() {
    let err = check(&specs(&["tcp:127.0.0.1:80:10.0.0.1:80"; 2])).unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].first, 0);
    assert_eq!(err.0[0].second, 1);
    assert_eq!(err.0[0].ports, vec![PortRange::single(80)]);
}

#[test]
fn wildcard_collides_with_specific_address() {
    let err = check(&specs(&[
        "tcp:*:80:10.0.0.1:80",
        "tcp:127.0.0.1:80:10.0.0.1:80",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
    let err = check(&specs(&[
        "tcp:[::1]:80:10.0.0.1:80",
        "tcp:[::]:80:10.0.0.1:80",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
}

#[test]
fn overlapping_ranges_are_merged_in_report() {
    let err = check(&specs(&[
        "tcp:0.0.0.0:100-200:10.0.0.1:100",
        "tcp:0.0.0.0:150-250:10.0.0.1:1000",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].protocol, Protocol::Tcp);
    assert_eq!(err.0[0].ports, vec![PortRange::new(150, 200).unwrap()]);
}

#[test]
fn every_collision_is_reported() {
    let err = check(&specs(&[
        "*:80:10.0.0.1:80",
        "*:80:10.0.0.1:80",
        "udp:[::]:80:10.0.0.1:80",
    ]))
    .unwrap_err();
    // #1/#2: tcp v4, tcp v6, udp v4, udp v6; #1/#3 and #2/#3: udp v6.
    assert_eq!(err.0.len(), 6);
    let text = err.to_string();
    assert!(
        text.starts_with("6 conflicting forwarding rule pair(s):"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 7);
}

#[test]
fn different_protocols_do_not_collide() {
    check(&specs(&["tcp:*:80:10.0.0.1:80", "udp:*:80:10.0.0.1:80"])).unwrap();
}

#[test]
fn report_mentions_both_rules() {
    let err = check(&specs(&[
        "tcp:*:80:10.0.0.1:80",
        "tcp:127.0.0.1:80:10.0.0.1:81",
    ]))
    .unwrap_err();
    assert_eq!(
        err.0[0].to_string(),
        "rule #1 'tcp:*:80:10.0.0.1:80' and rule #2 'tcp:127.0.0.1:80:10.0.0.1:81' \
         both bind tcp port 80 (0.0.0.0 vs 127.0.0.1)"
    );
}

#[test]
fn non_contiguous_ports_are_listed() {
    let err = check(&specs(&[
        "tcp:127.0.0.1:10-20:10.0.0.1:10",
        "tcp:127.0.0.1:12:10.0.0.1:1",
        "tcp:127.0.0.1:15-16:10.0.0.1:1",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 2);
    assert_eq!(err.0[0].ports, vec![PortRange::single(12)]);
    assert_eq!(err.0[1].ports, vec![PortRange::new(15, 16).unwrap()]);
}

#[test]
fn large_ranges_are_checked_quickly() {
    let start = std::time::Instant::now();
    let err = check(&specs(&["*:1-65535:10.0.0.1:1", "*:1-65535:10.0.0.2:1"])).unwrap_err();
    assert_eq!(err.0.len(), 4);
    assert!(
        err.0
            .iter()
            .all(|c| c.ports == vec![PortRange::new(1, 65535).unwrap()])
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
}
