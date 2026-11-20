use mongoose::conflict::{ConflictError, check};
use mongoose::spec::{ForwardSpec, PortRange, Protocol};

fn specs(inputs: &[&str]) -> Vec<ForwardSpec> {
    inputs.iter().map(|s| s.parse().unwrap()).collect()
}

/// For specs without an explicit `[::]` the IPv6 default must not matter.
fn check_both(specs: &[ForwardSpec]) -> Result<(), ConflictError> {
    let dual = check(specs, true);
    assert_eq!(dual, check(specs, false));
    dual
}

#[test]
fn disjoint_specs_pass() {
    check_both(&specs(&[
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
fn explicit_ipv6_unspecified_follows_the_system_default() {
    for other in [
        "tcp:0.0.0.0:80:10.0.0.1:80",
        "tcp:127.0.0.1:80:10.0.0.1:80",
        "tcp:*:80:10.0.0.1:80",
    ] {
        let rules = specs(&["tcp:[::]:80:10.0.0.1:80", other]);
        let err = check(&rules, true).unwrap_err();
        assert!(
            err.0
                .iter()
                .any(|c| c.first_ip.is_ipv6() && c.second_ip.is_ipv4()),
            "{other}: {err}"
        );
        // IPv6-only default: [::] only collides with the wildcard's [::].
        match check(&rules, false) {
            Ok(()) => assert!(!other.starts_with("tcp:*")),
            Err(err) => assert!(err.0.iter().all(|c| c.second_ip.is_ipv6()), "{err}"),
        }
    }
}

#[test]
fn wildcard_ipv6_half_never_reaches_ipv4() {
    check_both(&specs(&[
        "tcp:[::1]:80:10.0.0.1:80",
        "tcp:127.0.0.1:80:10.0.0.1:80",
    ]))
    .unwrap();
    let err = check_both(&specs(&[
        "tcp:*:80:10.0.0.1:80",
        "tcp:127.0.0.1:80:10.0.0.1:80",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert!(err.0[0].first_ip.is_ipv4());
}

#[test]
fn dual_stack_report_names_both_addresses() {
    let err = check(
        &specs(&["udp:[::]:53:10.0.0.1:53", "udp:10.0.0.2:53:10.0.0.1:53"]),
        true,
    )
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert!(
        err.0[0].to_string().ends_with("([::] vs 10.0.0.2)"),
        "{}",
        err.0[0]
    );
}

#[test]
fn ephemeral_ports_never_collide() {
    check_both(&specs(&[
        "tcp:127.0.0.1:0:10.0.0.1:80",
        "tcp:127.0.0.1:0:10.0.0.1:80",
    ]))
    .unwrap();
}

#[test]
fn identical_specs_collide() {
    let err = check_both(&specs(&["tcp:127.0.0.1:80:10.0.0.1:80"; 2])).unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].first, 0);
    assert_eq!(err.0[0].second, 1);
    assert_eq!(err.0[0].ports, PortRange::single(80));
}

#[test]
fn wildcard_collides_with_specific_address() {
    let err = check_both(&specs(&[
        "tcp:*:80:10.0.0.1:80",
        "tcp:127.0.0.1:80:10.0.0.1:80",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
    let err = check_both(&specs(&[
        "tcp:[::1]:80:10.0.0.1:80",
        "tcp:[::]:80:10.0.0.1:80",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
}

#[test]
fn overlapping_ranges_are_reported_as_one_range() {
    let err = check_both(&specs(&[
        "tcp:0.0.0.0:100-200:10.0.0.1:100",
        "tcp:0.0.0.0:150-250:10.0.0.1:1000",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].protocol, Protocol::Tcp);
    assert_eq!(err.0[0].ports, PortRange::new(150, 200).unwrap());
}

#[test]
fn every_collision_is_reported() {
    let rules = specs(&[
        "*:80:10.0.0.1:80",
        "*:80:10.0.0.1:80",
        "udp:[::]:80:10.0.0.1:80",
    ]);
    // #1/#2: tcp v4, tcp v6, udp v4, udp v6; #1/#3 and #2/#3: udp v6.
    let err = check(&rules, false).unwrap_err();
    assert_eq!(err.0.len(), 6);
    let text = err.to_string();
    assert!(
        text.starts_with("6 conflict(s) between forwarding rules:"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 7);
    // Dual-stack: the explicit [::] also hits both rules' 0.0.0.0.
    assert_eq!(check(&rules, true).unwrap_err().0.len(), 8);
}

#[test]
fn adjacent_ranges_do_not_collide() {
    check_both(&specs(&[
        "tcp:*:100-199:10.0.0.1:1",
        "tcp:*:200-299:10.0.0.1:1",
    ]))
    .unwrap();
}

#[test]
fn contained_range_collides() {
    let err = check_both(&specs(&[
        "tcp:0.0.0.0:100-199:10.0.0.1:1",
        "tcp:0.0.0.0:120:10.0.0.1:1",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].ports, PortRange::single(120));
}

#[test]
fn ephemeral_never_collides_with_ranges() {
    check_both(&specs(&[
        "tcp:127.0.0.1:0:10.0.0.1:1",
        "tcp:*:1-65535:10.0.0.1:1",
    ]))
    .unwrap();
}

#[test]
fn different_protocols_do_not_collide() {
    check_both(&specs(&["tcp:*:80:10.0.0.1:80", "udp:*:80:10.0.0.1:80"])).unwrap();
}

#[test]
fn report_mentions_both_rules() {
    let err = check_both(&specs(&[
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
fn each_pair_reports_its_own_range() {
    let err = check_both(&specs(&[
        "tcp:127.0.0.1:10-20:10.0.0.1:10",
        "tcp:127.0.0.1:12:10.0.0.1:1",
        "tcp:127.0.0.1:15-16:10.0.0.1:1",
    ]))
    .unwrap_err();
    assert_eq!(err.0.len(), 2);
    assert_eq!((err.0[0].first, err.0[0].second), (0, 1));
    assert_eq!(err.0[0].ports, PortRange::single(12));
    assert_eq!((err.0[1].first, err.0[1].second), (0, 2));
    assert_eq!(err.0[1].ports, PortRange::new(15, 16).unwrap());
}

#[test]
fn large_ranges_are_checked_quickly() {
    let start = std::time::Instant::now();
    let err = check_both(&specs(&["*:1-65535:10.0.0.1:1", "*:1-65535:10.0.0.2:1"])).unwrap_err();
    assert_eq!(err.0.len(), 4);
    assert!(
        err.0
            .iter()
            .all(|c| c.ports == PortRange::new(1, 65535).unwrap())
    );
    assert!(start.elapsed() < std::time::Duration::from_millis(50));
}
