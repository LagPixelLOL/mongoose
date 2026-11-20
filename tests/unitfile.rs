use mongoose::spec::ForwardSpec;
use mongoose::unitfile::{ParsedUnit, parse_foreign_unit, split_systemd_words};

fn spec(text: &str) -> ForwardSpec {
    text.parse().unwrap()
}

fn parse(unit: &str) -> ParsedUnit {
    parse_foreign_unit(unit).unwrap_or_else(|err| panic!("{err}\n---\n{unit}"))
}

fn error(unit: &str) -> String {
    parse_foreign_unit(unit).expect_err(unit)
}

fn service(exec: &str) -> String {
    format!("[Unit]\nDescription=x\n\n[Service]\nExecStart={exec}\n")
}

#[test]
fn minimal_hand_written_unit() {
    let unit = parse(&service("/usr/bin/mongoose start tcp:*:80:10.0.0.1:80"));
    assert_eq!(unit.exe.to_str(), Some("/usr/bin/mongoose"));
    assert_eq!(unit.specs, vec![spec("tcp:*:80:10.0.0.1:80")]);
    assert!(!unit.verbose);
    assert!(unit.notes.is_empty());
}

#[test]
fn several_rules_and_verbose_anywhere() {
    for exec in [
        "/usr/bin/mongoose -v start tcp:*:80:10.0.0.1:80 udp:*:53:10.0.0.1:53",
        "/usr/bin/mongoose start --verbose tcp:*:80:10.0.0.1:80 udp:*:53:10.0.0.1:53",
        "/usr/bin/mongoose start tcp:*:80:10.0.0.1:80 udp:*:53:10.0.0.1:53 -v",
    ] {
        let unit = parse(&service(exec));
        assert!(unit.verbose, "{exec}");
        assert_eq!(unit.specs.len(), 2, "{exec}");
    }
    // The same rules as on the command line: verbose at most once.
    assert!(
        error(&service(
            "/usr/bin/mongoose -v start -v tcp:*:80:10.0.0.1:80"
        ))
        .contains("cannot be used multiple times")
    );
}

#[test]
fn comments_blank_lines_and_whitespace_around_equals() {
    let unit = parse(
        "# a comment\n; another comment\n\n[Service]\n  # indented comment\n\
         Type = simple\nExecStart = /usr/bin/mongoose start tcp:*:80:10.0.0.1:80   \n",
    );
    assert_eq!(unit.specs, vec![spec("tcp:*:80:10.0.0.1:80")]);
}

#[test]
fn line_continuations_with_interleaved_comments() {
    let unit = parse(
        "[Service]\nExecStart=/usr/bin/mongoose start \\\n  tcp:*:80:10.0.0.1:80 \\\n\
         # comment inside the continuation\n  udp:*:53:10.0.0.1:53\nRestart=always\n",
    );
    assert_eq!(
        unit.specs,
        vec![spec("tcp:*:80:10.0.0.1:80"), spec("udp:*:53:10.0.0.1:53")]
    );
    // A continuation at the very end of the file.
    let unit = parse("[Service]\nExecStart=/usr/bin/mongoose start tcp:*:80:10.0.0.1:80 \\");
    assert_eq!(unit.specs.len(), 1);
}

#[test]
fn crlf_and_byte_order_mark() {
    let unit =
        parse("\u{feff}[Service]\r\nExecStart=/usr/bin/mongoose start tcp:*:80:10.0.0.1:80\r\n");
    assert_eq!(unit.specs, vec![spec("tcp:*:80:10.0.0.1:80")]);
}

#[test]
fn exec_start_reset_and_multiple_sections() {
    // An empty assignment resets the list; later sections are merged.
    let unit = parse(
        "[Service]\nExecStart=/usr/bin/old start tcp:*:1:10.0.0.1:1\nExecStart=\n\
         [Install]\nWantedBy=multi-user.target\n\
         [Service]\nExecStart=/usr/bin/mongoose start tcp:*:2:10.0.0.1:2\n",
    );
    assert_eq!(unit.exe.to_str(), Some("/usr/bin/mongoose"));
    assert_eq!(unit.specs, vec![spec("tcp:*:2:10.0.0.1:2")]);

    let err = error(
        "[Service]\nExecStart=/usr/bin/mongoose start tcp:*:1:10.0.0.1:1\n\
         ExecStart=/usr/bin/mongoose start tcp:*:2:10.0.0.1:2\n",
    );
    assert!(err.contains("2 ExecStart= commands"), "{err}");
}

#[test]
fn only_exact_service_section_and_key_count() {
    for unit in [
        "[Unit]\nExecStart=/usr/bin/mongoose start tcp:*:1:10.0.0.1:1\n",
        "[service]\nExecStart=/usr/bin/mongoose start tcp:*:1:10.0.0.1:1\n",
        "[Service]\nexecstart=/usr/bin/mongoose start tcp:*:1:10.0.0.1:1\n",
        "[Service]\nExecStartPre=/usr/bin/mongoose start tcp:*:1:10.0.0.1:1\n",
        "ExecStart=/usr/bin/mongoose start tcp:*:1:10.0.0.1:1\n",
        "",
    ] {
        assert!(error(unit).contains("no ExecStart="), "{unit:?}");
    }
}

#[test]
fn exec_start_prefixes() {
    let unit = parse(&service("-/usr/bin/mongoose start tcp:*:80:10.0.0.1:80"));
    assert_eq!(unit.exe.to_str(), Some("/usr/bin/mongoose"));
    assert!(unit.notes[0].contains("'-'"), "{:?}", unit.notes);

    // '@': the second word is argv[0], not an argument.
    let unit = parse(&service(
        "@/usr/bin/mongoose fancy-name start tcp:*:80:10.0.0.1:80",
    ));
    assert_eq!(unit.exe.to_str(), Some("/usr/bin/mongoose"));
    assert_eq!(unit.specs.len(), 1);
    assert!(error(&service("@/usr/bin/mongoose")).contains("argv[0]"));

    let unit = parse(&service("+-/usr/bin/mongoose start tcp:*:80:10.0.0.1:80"));
    assert_eq!(unit.notes.len(), 2);
    let unit = parse(&service("!!/usr/bin/mongoose start tcp:*:80:10.0.0.1:80"));
    assert_eq!(unit.notes.len(), 1);

    // ':' turns off environment variable expansion.
    let unit = parse(&service(":/opt/$dir/mongoose start tcp:*:80:10.0.0.1:80"));
    assert_eq!(unit.exe.to_str(), Some("/opt/$dir/mongoose"));
    assert!(
        error(&service("/opt/$dir/mongoose start tcp:*:80:10.0.0.1:80"))
            .contains("environment variable")
    );
}

#[test]
fn quoting_and_escapes() {
    let unit = parse(&service(
        "\"/opt/my dir/mongoose\" start 'tcp:*:80:10.0.0.1:80' tcp:\"*\":81:10.0.0.1:81",
    ));
    assert_eq!(unit.exe.to_str(), Some("/opt/my dir/mongoose"));
    assert_eq!(
        unit.specs,
        vec![spec("tcp:*:80:10.0.0.1:80"), spec("tcp:*:81:10.0.0.1:81")]
    );
    let unit = parse(&service(
        "/opt/a\\sb/\\x6dongoose start tcp:*:80:10.0.0.1:80",
    ));
    assert_eq!(unit.exe.to_str(), Some("/opt/a b/mongoose"));
    assert_eq!(
        split_systemd_words(r#"a "it's" 'say "hi"' \"x\""#, true).unwrap(),
        vec!["a", "it's", "say \"hi\"", "\"x\""]
    );
}

#[test]
fn specifiers_and_variables_are_not_guessed() {
    assert_eq!(
        parse(&service("/opt/100%%/mongoose start tcp:*:80:10.0.0.1:80"))
            .exe
            .to_str(),
        Some("/opt/100%/mongoose")
    );
    assert_eq!(
        parse(&service("/opt/$$x/mongoose start tcp:*:80:10.0.0.1:80"))
            .exe
            .to_str(),
        Some("/opt/$x/mongoose")
    );
    for exec in [
        "%h/bin/mongoose start tcp:*:80:10.0.0.1:80",
        "/usr/bin/mongoose start tcp:*:80:%i:80",
    ] {
        assert!(error(&service(exec)).contains("specifier"), "{exec}");
    }
    for exec in [
        "/usr/bin/mongoose start $RULES",
        "/usr/bin/mongoose start ${RULES}",
    ] {
        assert!(
            error(&service(exec)).contains("environment variable"),
            "{exec}"
        );
    }
}

#[test]
fn malformed_command_lines() {
    for (exec, needle) in [
        (
            "/usr/bin/mongoose start 'tcp:*:80:10.0.0.1:80",
            "unterminated quote",
        ),
        ("/usr/bin/mongoose start \\q", "unsupported escape"),
        ("/usr/bin/mongoose start \\x4", "invalid escape"),
        (
            "/bin/true ; /usr/bin/mongoose start tcp:*:80:10.0.0.1:80",
            "';'",
        ),
        ("-", "no command"),
    ] {
        let err = error(&service(exec));
        assert!(err.contains(needle), "{exec}: {err}");
    }
    // At the end of a line a backslash continues the line, so a trailing
    // backslash can only occur within the word splitter's input.
    assert!(
        split_systemd_words("a \\", true)
            .unwrap_err()
            .contains("trailing backslash")
    );
    // A quoted ';' is an ordinary argument.
    assert_eq!(split_systemd_words("a ';'", true).unwrap(), vec!["a", ";"]);
}

#[test]
fn not_a_mongoose_start_command_line() {
    for (exec, needle) in [
        (
            "/usr/bin/mongoose setup tcp:*:80:10.0.0.1:80",
            "`mongoose setup`",
        ),
        ("/usr/bin/mongoose remove", "`mongoose remove`"),
        ("/usr/bin/mongoose show", "`mongoose show`"),
        ("/usr/bin/mongoose", "not a valid mongoose command line"),
        (
            "/usr/bin/mongoose start",
            "not a valid mongoose command line",
        ),
        (
            "/usr/bin/socat TCP-LISTEN:80 TCP:10.0.0.1:80",
            "not a valid mongoose command line",
        ),
        (
            "/usr/bin/mongoose start --bogus tcp:*:80:10.0.0.1:80",
            "not a valid mongoose command line",
        ),
        (
            "/usr/bin/mongoose start tcp:*:80:*:80",
            "not a valid mongoose command line",
        ),
    ] {
        let err = error(&service(exec));
        assert!(err.contains(needle), "{exec}: {err}");
    }
}

#[test]
fn colliding_rules_still_parse() {
    // Interpretation is separate from validation; show reports collisions.
    let unit = parse(&service(
        "/usr/bin/mongoose start tcp:*:80:10.0.0.1:80 tcp:*:80:10.0.0.2:80",
    ));
    assert_eq!(unit.specs.len(), 2);
}
