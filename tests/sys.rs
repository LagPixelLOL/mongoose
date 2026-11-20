use mongoose::sys::{is_mount_point, parse_mount_points, parse_port_list, parse_port_range_pair};

#[test]
fn port_range_pair() {
    assert_eq!(
        parse_port_range_pair("32768\t60999\n"),
        Some((32768, 60999))
    );
    assert_eq!(parse_port_range_pair("1024 65535"), Some((1024, 65535)));
    assert_eq!(parse_port_range_pair("60999 32768"), None);
    assert_eq!(parse_port_range_pair("32768"), None);
    assert_eq!(parse_port_range_pair("1 2 3"), None);
    assert_eq!(parse_port_range_pair(""), None);
}

#[test]
fn port_list() {
    assert_eq!(parse_port_list("\n"), Some(vec![]));
    assert_eq!(parse_port_list("8080\n"), Some(vec![(8080, 8080)]));
    assert_eq!(
        parse_port_list("8080,9000-9100\n"),
        Some(vec![(8080, 8080), (9000, 9100)])
    );
    assert_eq!(parse_port_list("9100-9000"), None);
    assert_eq!(parse_port_list("x"), None);
}

#[test]
fn fd_limit_is_raised_to_hard_limit() {
    let limit = mongoose::sys::raise_fd_limit().unwrap();
    assert!(limit >= 1024);
}

#[test]
fn mount_points_from_mountinfo() {
    let text = "18 17 0:20 / / rw - overlay none rw\n\
                33 46 0:28 / /run rw,nosuid shared:12 - tmpfs tmpfs rw\n\
                40 18 8:0 /units/x /etc/systemd/system/my\\040unit.service rw - ext4 /dev/sda rw\n";
    assert_eq!(
        parse_mount_points(text),
        vec![
            std::path::PathBuf::from("/"),
            "/run".into(),
            "/etc/systemd/system/my unit.service".into()
        ]
    );
}

#[test]
fn plain_files_are_not_mount_points() {
    assert!(is_mount_point(std::path::Path::new("/")).unwrap());
    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("f");
    std::fs::write(&file, b"x").unwrap();
    assert!(!is_mount_point(&file).unwrap());
}
