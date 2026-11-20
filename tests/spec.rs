use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use mongoose::spec::{
    Binding, ForwardSpec, LocalAddr, PortRange, Protocol, Protocols, SpecError, parse,
};

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

fn err(input: &str) -> SpecError {
    parse(input).expect_err(input)
}

#[test]
fn parses_full_tcp_spec() {
    let spec = parse("tcp:127.0.0.1:8080:10.0.0.1:80").unwrap();
    assert_eq!(spec.protocols, Protocols::Tcp);
    assert_eq!(spec.local_addr, LocalAddr::Ip(v4(127, 0, 0, 1)));
    assert_eq!(spec.local_ports, PortRange::single(8080));
    assert_eq!(spec.target_ip, v4(10, 0, 0, 1));
    assert_eq!(spec.target_ports, PortRange::single(80));
}

#[test]
fn no_prefix_means_both_protocols() {
    let spec = parse("1.2.3.4:53:8.8.8.8:53").unwrap();
    assert_eq!(spec.protocols, Protocols::Both);
    assert_eq!(spec.protocols.list(), &[Protocol::Tcp, Protocol::Udp]);
}

#[test]
fn udp_prefix() {
    assert_eq!(
        parse("udp:*:53:8.8.8.8:53").unwrap().protocols,
        Protocols::Udp
    );
}

#[test]
fn protocol_prefix_is_case_sensitive() {
    assert_eq!(
        err("TCP:*:1:1.1.1.1:1"),
        SpecError::UnknownProtocol("TCP".into())
    );
    assert_eq!(
        err("sctp:*:1:1.1.1.1:1"),
        SpecError::UnknownProtocol("sctp".into())
    );
}

#[test]
fn wildcard_binds_both_families() {
    let spec = parse("*:80:10.0.0.1:80").unwrap();
    assert_eq!(spec.local_addr, LocalAddr::Any);
    assert_eq!(
        spec.local_addr.ips(),
        vec![
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        ]
    );
}

#[test]
fn ipv6_unspecified_binds_only_ipv6() {
    let spec = parse("[::]:80:10.0.0.1:80").unwrap();
    assert_eq!(
        spec.local_addr.ips(),
        vec![IpAddr::V6(Ipv6Addr::UNSPECIFIED)]
    );
}

#[test]
fn ipv4_unspecified_binds_only_ipv4() {
    let spec = parse("0.0.0.0:80:10.0.0.1:80").unwrap();
    assert_eq!(
        spec.local_addr.ips(),
        vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)]
    );
}

#[test]
fn ipv6_addresses_in_brackets() {
    let spec = parse("tcp:[2001:db8::1]:443:[2001:db8::2]:8443").unwrap();
    assert_eq!(
        spec.local_addr,
        LocalAddr::Ip("2001:db8::1".parse().unwrap())
    );
    assert_eq!(spec.target_ip, "2001:db8::2".parse::<IpAddr>().unwrap());
}

#[test]
fn cross_family_forwarding_is_allowed() {
    assert!(parse("[::]:80:10.0.0.1:80").is_ok());
    assert!(parse("0.0.0.0:80:[2001:db8::1]:80").is_ok());
}

#[test]
fn unbracketed_ipv6_is_rejected() {
    for input in [
        "::1:80:1.1.1.1:80",
        "tcp:::1:80:1.1.1.1:80",
        "1.1.1.1:80:::1:80",
        "1.1.1.1:80:2001:db8::1:80",
        "fe80::1:80:1.1.1.1:80",
        "dead:beef::1:80:1.1.1.1:80",
    ] {
        assert_eq!(err(input), SpecError::UnbracketedIpv6, "{input}");
    }
}

#[test]
fn brackets_only_for_ipv6() {
    assert_eq!(
        err("[1.2.3.4]:80:1.1.1.1:80"),
        SpecError::BracketedNonIpv6("1.2.3.4".into())
    );
}

#[test]
fn wildcard_target_is_rejected() {
    assert_eq!(err("*:80:*:80"), SpecError::WildcardTarget);
}

#[test]
fn bracketed_wildcard_is_not_a_wildcard() {
    assert!(matches!(
        err("[*]:80:1.1.1.1:80"),
        SpecError::InvalidAddress { .. }
    ));
}

#[test]
fn unusable_addresses_are_rejected() {
    assert!(matches!(
        err("*:80:0.0.0.0:80"),
        SpecError::UnusableAddress {
            role: "target",
            kind: "unspecified",
            ..
        }
    ));
    assert!(matches!(
        err("*:80:[::]:80"),
        SpecError::UnusableAddress {
            role: "target",
            kind: "unspecified",
            ..
        }
    ));
    assert!(matches!(
        err("224.0.0.1:80:1.1.1.1:80"),
        SpecError::UnusableAddress {
            role: "local",
            kind: "multicast",
            ..
        }
    ));
    assert!(matches!(
        err("*:80:255.255.255.255:80"),
        SpecError::UnusableAddress {
            role: "target",
            kind: "broadcast",
            ..
        }
    ));
    assert!(matches!(
        err("*:80:[ff02::1]:80"),
        SpecError::UnusableAddress {
            kind: "multicast",
            ..
        }
    ));
}

#[test]
fn ipv4_mapped_ipv6_is_rejected() {
    assert!(matches!(
        err("*:80:[::ffff:1.2.3.4]:80"),
        SpecError::MappedIpv6 { role: "target", .. }
    ));
    assert!(matches!(
        err("[::ffff:127.0.0.1]:80:1.2.3.4:80"),
        SpecError::MappedIpv6 { role: "local", .. }
    ));
}

#[test]
fn hostnames_are_rejected() {
    assert!(matches!(
        err("*:80:example.com:80"),
        SpecError::InvalidAddress { .. }
    ));
}

#[test]
fn ranges_are_inclusive() {
    let spec = parse("*:10000-10099:10.0.0.1:20000-20099").unwrap();
    assert_eq!(spec.local_ports.count(), 100);
    assert_eq!(spec.local_ports.start(), 10000);
    assert_eq!(spec.local_ports.end(), 10099);
    assert_eq!(spec.target_ports.end(), 20099);
}

#[test]
fn single_target_port_expands_to_range() {
    let spec = parse("*:10000-10099:10.0.0.1:20000").unwrap();
    assert_eq!(spec.target_ports, PortRange::new(20000, 20099).unwrap());
}

#[test]
fn single_target_port_overflow_is_rejected() {
    assert_eq!(
        err("*:100-200:10.0.0.1:65500"),
        SpecError::TargetRangeOverflow {
            start: 65500,
            len: 101
        }
    );
    assert!(parse("*:100-135:10.0.0.1:65500").is_ok());
}

#[test]
fn range_length_must_match() {
    assert_eq!(
        err("*:100-199:10.0.0.1:200-298"),
        SpecError::RangeLengthMismatch {
            local: 100,
            target: 99
        }
    );
    assert_eq!(
        err("*:100:10.0.0.1:200-201"),
        SpecError::RangeLengthMismatch {
            local: 1,
            target: 2
        }
    );
}

#[test]
fn zero_port_only_in_single_local_mode() {
    let spec = parse("tcp:127.0.0.1:0:10.0.0.1:80").unwrap();
    assert_eq!(spec.local_ports, PortRange::single(0));
    assert_eq!(
        err("*:0-10:10.0.0.1:80"),
        SpecError::ZeroInRange { role: "local" }
    );
    assert_eq!(
        err("*:10-20:10.0.0.1:0-10"),
        SpecError::ZeroInRange { role: "target" }
    );
    assert_eq!(err("*:80:10.0.0.1:0"), SpecError::ZeroTargetPort);
}

#[test]
fn reversed_range_is_rejected() {
    assert!(matches!(
        err("*:200-100:10.0.0.1:80"),
        SpecError::ReversedRange { .. }
    ));
}

#[test]
fn malformed_ports_are_rejected() {
    for input in [
        "*:+80:1.1.1.1:80",
        "*:080:1.1.1.1:80",
        "*:65536:1.1.1.1:80",
        "*:80 :1.1.1.1:80",
        "*::1.1.1.1:80",
        "*:1-2-3:1.1.1.1:80",
        "*:80:1.1.1.1:",
        "*:80:1.1.1.1:x",
        "*:-5:1.1.1.1:80",
    ] {
        assert!(parse(input).is_err(), "{input}");
    }
}

#[test]
fn malformed_structure_is_rejected() {
    for input in [
        "",
        "*",
        "*:80",
        "*:80:1.1.1.1",
        "tcp:*:80:1.1.1.1",
        "*:80:1.1.1.1:80:90",
        "[::1:80:1.1.1.1:80",
        "[::1]80:1.1.1.1:80",
        " *:80:1.1.1.1:80",
    ] {
        assert!(parse(input).is_err(), "{input:?}");
    }
}

#[test]
fn display_round_trips() {
    for input in [
        "tcp:127.0.0.1:8080:10.0.0.1:80",
        "udp:*:53:[2001:db8::1]:5353",
        "*:10000-10099:10.0.0.1:20000-20099",
        "[::]:443:192.168.1.1:443",
        "tcp:127.0.0.1:0:10.0.0.1:80",
    ] {
        let spec = parse(input).unwrap();
        assert_eq!(spec.to_string(), input);
        assert_eq!(parse(&spec.to_string()).unwrap(), spec);
    }
}

#[test]
fn display_normalises_single_target_port() {
    let spec = parse("*:100-101:10.0.0.1:200").unwrap();
    assert_eq!(spec.to_string(), "*:100-101:10.0.0.1:200-201");
}

#[test]
fn from_str_matches_parse() {
    let spec: ForwardSpec = "tcp:*:1:1.1.1.1:1".parse().unwrap();
    assert_eq!(spec, parse("tcp:*:1:1.1.1.1:1").unwrap());
}

#[test]
fn bindings_expand_everything() {
    let spec = parse("*:100-101:10.0.0.1:200").unwrap();
    let bindings = spec.bindings();
    assert_eq!(bindings.len(), 8);
    assert_eq!(spec.listener_count(), 8);
    let addr = |ip: IpAddr, port| SocketAddr::new(ip, port);
    assert!(bindings.contains(&Binding {
        protocol: Protocol::Udp,
        local: addr(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 101),
        target: addr(v4(10, 0, 0, 1), 201),
        ipv6_only: true,
    }));
    assert!(bindings.contains(&Binding {
        protocol: Protocol::Tcp,
        local: addr(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 100),
        target: addr(v4(10, 0, 0, 1), 200),
        ipv6_only: false,
    }));
}

#[test]
fn only_the_wildcard_ipv6_half_is_forced_ipv6_only() {
    let forced = |input: &str| -> Vec<bool> {
        parse(input)
            .unwrap()
            .bindings()
            .iter()
            .map(|b| b.ipv6_only)
            .collect()
    };
    assert_eq!(forced("tcp:*:80:10.0.0.1:80"), vec![false, true]);
    assert_eq!(forced("tcp:[::]:80:10.0.0.1:80"), vec![false]);
    assert_eq!(forced("tcp:[2001:db8::1]:80:10.0.0.1:80"), vec![false]);
    assert_eq!(forced("tcp:0.0.0.0:80:10.0.0.1:80"), vec![false]);
}

#[test]
fn describe_is_one_expanded_line() {
    assert_eq!(
        parse("*:80-81:10.0.0.1:8080").unwrap().describe(true),
        "tcp+udp 0.0.0.0:80-81, [::]:80-81 -> 10.0.0.1:8080-8081 (2 ports, 8 sockets)"
    );
    assert_eq!(
        parse("tcp:127.0.0.1:0:[::1]:22").unwrap().describe(true),
        "tcp 127.0.0.1:0 -> [::1]:22 (ephemeral local port, 1 socket)"
    );
    assert_eq!(
        parse("udp:[2001:db8::1]:53:1.1.1.1:53")
            .unwrap()
            .describe(true),
        "udp [2001:db8::1]:53 -> 1.1.1.1:53 (1 port, 1 socket)"
    );
}

#[test]
fn describe_states_the_ipv6_default_for_explicit_unspecified() {
    let spec = parse("udp:[::]:53:1.1.1.1:53").unwrap();
    assert_eq!(
        spec.describe(true),
        "udp [::]:53 (dual-stack, also IPv4 per system default) -> 1.1.1.1:53 (1 port, 1 socket)"
    );
    assert_eq!(
        spec.describe(false),
        "udp [::]:53 (IPv6 only per system default) -> 1.1.1.1:53 (1 port, 1 socket)"
    );
    // The wildcard's IPv6 half never follows the default.
    assert_eq!(
        parse("udp:*:53:1.1.1.1:53").unwrap().describe(true),
        "udp 0.0.0.0:53, [::]:53 -> 1.1.1.1:53 (1 port, 2 sockets)"
    );
}
