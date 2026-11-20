use clap::error::ErrorKind;
use mongoose::cli::{Cli, Command};

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_args(std::iter::once("mongoose").chain(args.iter().copied()))
}

#[test]
fn start_takes_specs() {
    let cli = parse(&["start", "tcp:*:80:10.0.0.1:80", "udp:*:53:10.0.0.1:53"]).unwrap();
    assert!(!cli.verbose());
    let Command::Start { specs, .. } = cli.command else {
        panic!("expected start");
    };
    assert_eq!(specs.len(), 2);
}

#[test]
fn start_requires_a_spec() {
    let err = parse(&["start"]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    let err = parse(&["setup"]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn invalid_spec_is_a_usage_error() {
    let err = parse(&["start", "tcp:*:80:*:80"]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ValueValidation);
}

#[test]
fn setup_skip_enable() {
    for flag in ["-s", "--skip-enable"] {
        let cli = parse(&["setup", flag, "*:80:10.0.0.1:80"]).unwrap();
        let Command::Setup { skip_enable, .. } = cli.command else {
            panic!("expected setup");
        };
        assert!(skip_enable);
    }
    let cli = parse(&["setup", "*:80:10.0.0.1:80"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Setup {
            skip_enable: false,
            ..
        }
    ));
    assert!(parse(&["setup", "-s", "-s", "*:80:10.0.0.1:80"]).is_err());
}

#[test]
fn remove_and_show_take_no_specs() {
    assert!(matches!(
        parse(&["remove"]).unwrap().command,
        Command::Remove { .. }
    ));
    assert!(matches!(
        parse(&["show"]).unwrap().command,
        Command::Show { .. }
    ));
    assert!(parse(&["remove", "*:80:10.0.0.1:80"]).is_err());
    assert!(parse(&["show", "*:80:10.0.0.1:80"]).is_err());
}

#[test]
fn verbose_on_every_command_and_position() {
    let cases: &[&[&str]] = &[
        &["-v", "start", "*:80:10.0.0.1:80"],
        &["--verbose", "start", "*:80:10.0.0.1:80"],
        &["start", "-v", "*:80:10.0.0.1:80"],
        &["start", "*:80:10.0.0.1:80", "--verbose"],
        &["setup", "-v", "*:80:10.0.0.1:80"],
        &["-v", "setup", "-s", "*:80:10.0.0.1:80"],
        &["remove", "-v"],
        &["-v", "remove"],
        &["show", "--verbose"],
        &["-v", "show"],
    ];
    for args in cases {
        assert!(parse(args).unwrap().verbose(), "{args:?}");
    }
}

#[test]
fn verbose_only_once() {
    let cases: &[&[&str]] = &[
        &["-v", "-v", "start", "*:80:10.0.0.1:80"],
        &["-vv", "start", "*:80:10.0.0.1:80"],
        &["start", "-vv", "*:80:10.0.0.1:80"],
        &["start", "-v", "--verbose", "*:80:10.0.0.1:80"],
        &["-v", "start", "-v", "*:80:10.0.0.1:80"],
        &["--verbose", "remove", "--verbose"],
        &["-v", "show", "-v"],
    ];
    for args in cases {
        let err = parse(args).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{args:?}: {err}");
        assert!(
            err.to_string().contains("cannot be used multiple times"),
            "{err}"
        );
    }
}

#[test]
fn unknown_command_is_rejected() {
    assert!(parse(&["stop"]).is_err());
    assert!(parse(&[]).is_err());
}
