use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use mongoose::service::{
    DROP_IN_DIR_NAME, MARKER, ServiceError, ServiceManager, SetupOutcome, System, UNIT_NAME,
    UnitConflictKind, UnitFileStatus, UnitState, is_managed, parse_unit, quote_exec_arg,
    render_unit, split_exec_args,
};
use mongoose::spec::ForwardSpec;
use tempfile::TempDir;

struct FakeSystem {
    root: bool,
    systemd: bool,
    fail: Option<&'static str>,
    show_output: String,
    calls: RefCell<Vec<String>>,
}

impl Default for FakeSystem {
    fn default() -> Self {
        Self {
            root: true,
            systemd: true,
            fail: None,
            show_output: String::new(),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl System for FakeSystem {
    fn is_root(&self) -> bool {
        self.root
    }

    fn uses_systemd(&self) -> bool {
        self.systemd
    }

    fn systemctl(&self, args: &[&str]) -> Result<String, ServiceError> {
        let command = args.join(" ");
        self.calls.borrow_mut().push(command.clone());
        if self.fail == Some(args[0]) {
            return Err(ServiceError::Systemctl {
                command,
                detail: "boom".into(),
            });
        }
        Ok(if args[0] == "show" {
            self.show_output.clone()
        } else {
            String::new()
        })
    }
}

struct Env {
    _tmp: TempDir,
    unit_dir: PathBuf,
    other_dir: PathBuf,
    exe: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let unit_dir = tmp.path().join("etc");
        let other_dir = tmp.path().join("lib");
        fs::create_dir_all(&unit_dir).unwrap();
        fs::create_dir_all(&other_dir).unwrap();
        let exe = tmp.path().join("bin dir").join("mongoose");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        Self {
            _tmp: tmp,
            unit_dir,
            other_dir,
            exe,
        }
    }

    fn manager(&self, system: FakeSystem) -> ServiceManager<FakeSystem> {
        ServiceManager::new(system, &self.unit_dir)
            .with_other_unit_dirs(vec![self.other_dir.clone()])
    }

    fn unit_path(&self) -> PathBuf {
        self.unit_dir.join(UNIT_NAME)
    }
}

fn specs(inputs: &[&str]) -> Vec<ForwardSpec> {
    inputs.iter().map(|s| s.parse().unwrap()).collect()
}

fn calls(manager: &ServiceManager<FakeSystem>) -> Vec<String> {
    manager.system().calls.borrow().clone()
}

#[test]
fn unit_contains_only_non_defaults() {
    let unit = render_unit(
        Path::new("/usr/local/bin/mongoose"),
        &specs(&["tcp:*:80:10.0.0.1:80", "udp:[::]:53:[2001:db8::1]:53"]),
    );
    assert_eq!(
        unit,
        format!(
            "{MARKER}\n\
             [Unit]\n\
             Description=Mongoose port forwarder\n\
             Wants=network-online.target\n\
             After=network-online.target\n\
             \n\
             [Service]\n\
             ExecStart=/usr/local/bin/mongoose start tcp:*:80:10.0.0.1:80 udp:[::]:53:[2001:db8::1]:53\n\
             Restart=always\n\
             RestartSec=3\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n"
        )
    );
    for default in ["User=", "Type=", "Group=", "RemainAfterExit", "KillMode"] {
        assert!(!unit.contains(default), "{default}");
    }
    assert!(is_managed(&unit));
}

#[test]
fn exec_args_quoting_round_trips() {
    for arg in [
        "/usr/bin/mongoose",
        "/opt/my dir/mongoose",
        "/weird/\"quoted\"/path",
        "/back\\slash",
        "/100%/x",
        "/$HOME/x",
        "tab\there",
        "*:80:[::1]:80",
    ] {
        let quoted = quote_exec_arg(arg);
        assert_eq!(
            split_exec_args(&quoted).unwrap(),
            vec![arg.to_string()],
            "{quoted}"
        );
    }
    assert_eq!(quote_exec_arg("/opt/my dir/m"), "\"/opt/my dir/m\"");
    assert_eq!(quote_exec_arg("/100%/x"), "/100%%/x");
    assert_eq!(quote_exec_arg("/$x"), "/$$x");
}

#[test]
fn split_exec_args_is_strict() {
    for line in [
        "\"unterminated",
        "a %n b",
        "a $HOME",
        "'single'",
        "\"a\"b",
        "a;b",
    ] {
        assert!(split_exec_args(line).is_err(), "{line}");
    }
    assert_eq!(
        split_exec_args("  a   \"b c\"  d ").unwrap(),
        vec!["a", "b c", "d"]
    );
}

#[test]
fn parse_unit_round_trips() {
    let rules = specs(&["*:10-20:10.0.0.1:30", "tcp:127.0.0.1:0:[::1]:22"]);
    let exe = Path::new("/opt/mongoose dir/mongoose");
    let (parsed_exe, parsed) = parse_unit(&render_unit(exe, &rules)).unwrap();
    assert_eq!(parsed_exe, exe);
    assert_eq!(parsed, rules);
}

#[test]
fn parse_unit_rejects_unexpected_content() {
    let base = render_unit(Path::new("/bin/mongoose"), &specs(&["*:1:1.1.1.1:1"]));
    let no_exec = base.replace("ExecStart=", "#ExecStart=");
    assert!(parse_unit(&no_exec).is_err());
    let twice = base.replace("Restart=always", "ExecStart=/bin/x start *:2:1.1.1.1:2");
    assert!(parse_unit(&twice).is_err());
    let not_start = base.replace(" start ", " remove ");
    assert!(parse_unit(&not_start).is_err());
    let bad_rule = base.replace("*:1:1.1.1.1:1", "*:1:*:1");
    assert!(parse_unit(&bad_rule).is_err());
}

#[test]
fn setup_installs_enables_and_starts() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    let rules = specs(&["*:80:10.0.0.1:80"]);
    let outcome = manager.setup(&env.exe, &rules, false).unwrap();
    assert_eq!(outcome, SetupOutcome::Installed);

    let contents = fs::read_to_string(env.unit_path()).unwrap();
    assert_eq!(contents, render_unit(&env.exe, &rules));
    let mode = fs::metadata(env.unit_path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644);
    assert_eq!(
        calls(&manager),
        vec![
            "daemon-reload",
            "enable mongoose.service",
            "restart mongoose.service"
        ]
    );
    // No temporary files are left behind.
    assert_eq!(fs::read_dir(&env.unit_dir).unwrap().count(), 1);
}

#[test]
fn setup_skip_enable_only_writes() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap();
    assert!(env.unit_path().is_file());
    assert!(calls(&manager).is_empty());
}

#[test]
fn setup_replaces_own_unit() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap();
    let rules = specs(&["*:81:10.0.0.1:81"]);
    let outcome = manager.setup(&env.exe, &rules, false).unwrap();
    assert_eq!(outcome, SetupOutcome::Replaced);
    assert_eq!(
        fs::read_to_string(env.unit_path()).unwrap(),
        render_unit(&env.exe, &rules)
    );
    assert_eq!(
        calls(&manager),
        vec![
            "daemon-reload",
            "enable mongoose.service",
            "restart mongoose.service"
        ]
    );
}

#[test]
fn setup_refuses_foreign_unit() {
    let env = Env::new();
    let foreign = "[Service]\nExecStart=/usr/bin/something\n";
    fs::write(env.unit_path(), foreign).unwrap();
    let manager = env.manager(FakeSystem::default());
    let err = manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), false)
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotManaged(_)), "{err}");
    assert_eq!(fs::read_to_string(env.unit_path()).unwrap(), foreign);
    assert!(calls(&manager).is_empty());
}

#[test]
fn marker_must_be_first_line() {
    let env = Env::new();
    fs::write(env.unit_path(), format!("[Unit]\n{MARKER}\n")).unwrap();
    let err = env
        .manager(FakeSystem::default())
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotManaged(_)));
}

#[test]
fn setup_refuses_symlinked_unit() {
    let env = Env::new();
    // A masked unit is a symlink to /dev/null.
    std::os::unix::fs::symlink("/dev/null", env.unit_path()).unwrap();
    let err = env
        .manager(FakeSystem::default())
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotManaged(_)), "{err}");
}

#[test]
fn setup_refuses_shadowing_units_and_drop_ins() {
    let env = Env::new();
    fs::write(env.other_dir.join(UNIT_NAME), "[Service]\n").unwrap();
    fs::create_dir(env.unit_dir.join(DROP_IN_DIR_NAME)).unwrap();
    let manager = env.manager(FakeSystem::default());
    let err = manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), false)
        .unwrap_err();
    let ServiceError::Conflicts(conflicts) = &err else {
        panic!("unexpected {err}");
    };
    assert_eq!(conflicts.len(), 2);
    assert!(
        conflicts
            .iter()
            .any(|c| c.kind == UnitConflictKind::OtherUnit)
    );
    assert!(conflicts.iter().any(|c| c.kind == UnitConflictKind::DropIn));
    assert!(!env.unit_path().exists());
    assert!(calls(&manager).is_empty());
}

#[test]
fn setup_requires_root_and_systemd() {
    let env = Env::new();
    let rules = specs(&["*:80:10.0.0.1:80"]);
    let not_root = env.manager(FakeSystem {
        root: false,
        ..Default::default()
    });
    assert!(matches!(
        not_root.setup(&env.exe, &rules, false),
        Err(ServiceError::NotRoot)
    ));
    let no_systemd = env.manager(FakeSystem {
        systemd: false,
        ..Default::default()
    });
    assert!(matches!(
        no_systemd.setup(&env.exe, &rules, false),
        Err(ServiceError::NoSystemd)
    ));
    assert!(!env.unit_path().exists());
}

#[test]
fn setup_requires_existing_absolute_executable() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    let rules = specs(&["*:80:10.0.0.1:80"]);
    for exe in [Path::new("mongoose"), Path::new("/nonexistent/mongoose")] {
        assert!(matches!(
            manager.setup(exe, &rules, false),
            Err(ServiceError::InvalidExecutable(_))
        ));
    }
}

#[test]
fn setup_reports_systemctl_failure() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        fail: Some("enable"),
        ..Default::default()
    });
    let err = manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), false)
        .unwrap_err();
    assert!(matches!(err, ServiceError::Systemctl { .. }));
    assert_eq!(
        calls(&manager),
        vec!["daemon-reload", "enable mongoose.service"]
    );
}

#[test]
fn remove_stops_disables_and_deletes() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap();
    manager.remove().unwrap();
    assert!(!env.unit_path().exists());
    assert_eq!(
        calls(&manager),
        vec![
            "stop mongoose.service",
            "disable mongoose.service",
            "daemon-reload"
        ]
    );
}

#[test]
fn remove_requires_installed_unit() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    assert!(matches!(
        manager.remove(),
        Err(ServiceError::NotInstalled(_))
    ));
    assert!(calls(&manager).is_empty());
}

#[test]
fn remove_refuses_foreign_unit() {
    let env = Env::new();
    fs::write(env.unit_path(), "[Service]\n").unwrap();
    let manager = env.manager(FakeSystem::default());
    assert!(matches!(manager.remove(), Err(ServiceError::NotManaged(_))));
    assert!(env.unit_path().exists());
    assert!(calls(&manager).is_empty());
}

#[test]
fn remove_refuses_when_another_unit_would_take_over() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap();
    fs::write(env.other_dir.join(UNIT_NAME), "[Service]\n").unwrap();
    assert!(matches!(manager.remove(), Err(ServiceError::Conflicts(_))));
    assert!(env.unit_path().exists());
}

#[test]
fn remove_leaves_drop_ins_in_place() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap();
    let drop_in = env.unit_dir.join(DROP_IN_DIR_NAME);
    fs::create_dir(&drop_in).unwrap();
    manager.remove().unwrap();
    assert!(!env.unit_path().exists());
    assert!(drop_in.is_dir());
}

#[test]
fn remove_stops_on_systemctl_failure() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        fail: Some("stop"),
        ..Default::default()
    });
    manager
        .setup(&env.exe, &specs(&["*:80:10.0.0.1:80"]), true)
        .unwrap();
    assert!(manager.remove().is_err());
    assert!(env.unit_path().exists());
}

#[test]
fn remove_requires_root() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        root: false,
        ..Default::default()
    });
    assert!(matches!(manager.remove(), Err(ServiceError::NotRoot)));
}

const SHOW_RUNNING: &str = "LoadState=loaded\nActiveState=active\nSubState=running\n\
    UnitFileState=enabled\nResult=success\nMainPID=4242\n\
    ActiveEnterTimestamp=Fri 2026-11-20 10:00:00 UTC\nNRestarts=2\nNeedDaemonReload=no\n";

#[test]
fn unit_state_parsing() {
    let state = UnitState::parse(SHOW_RUNNING);
    assert_eq!(state.load_state, "loaded");
    assert_eq!(state.active_state, "active");
    assert_eq!(state.sub_state, "running");
    assert_eq!(state.unit_file_state, "enabled");
    assert_eq!(state.main_pid, Some(4242));
    assert_eq!(state.restarts, Some(2));
    assert!(!state.need_daemon_reload);
    assert_eq!(
        state.active_since.as_deref(),
        Some("Fri 2026-11-20 10:00:00 UTC")
    );

    let stopped = UnitState::parse("MainPID=0\nActiveEnterTimestamp=\nNeedDaemonReload=yes\n");
    assert_eq!(stopped.main_pid, None);
    assert_eq!(stopped.active_since, None);
    assert!(stopped.need_daemon_reload);
}

#[test]
fn show_managed_unit() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        root: false,
        show_output: SHOW_RUNNING.into(),
        ..Default::default()
    });
    let rules = specs(&["tcp:*:80:10.0.0.1:8080"]);
    fs::write(env.unit_path(), render_unit(&env.exe, &rules)).unwrap();

    let report = manager.show().unwrap();
    assert_eq!(
        report.file,
        UnitFileStatus::Managed {
            exe: env.exe.clone(),
            specs: rules
        }
    );
    assert_eq!(report.state.as_ref().unwrap().main_pid, Some(4242));
    assert_eq!(
        calls(&manager),
        vec![format!(
            "show mongoose.service --property={}",
            mongoose::service::SHOW_PROPERTIES
        )]
    );

    let text = report.to_string();
    assert!(text.contains("Managed:      yes"), "{text}");
    assert!(text.contains("  #1 tcp:*:80:10.0.0.1:8080"), "{text}");
    assert!(
        text.contains("tcp 0.0.0.0:80, [::]:80 -> 10.0.0.1:8080 (1 port, 2 sockets)"),
        "{text}"
    );
    assert!(
        text.contains("Active:       active (running) since Fri 2026-11-20"),
        "{text}"
    );
    assert!(text.contains("Enabled:      enabled"), "{text}");
    assert!(text.contains("Main PID:     4242"), "{text}");
    assert!(text.contains("journalctl -u mongoose.service"), "{text}");
    assert!(text.contains("systemctl status mongoose.service"), "{text}");
}

#[test]
fn show_missing_and_foreign_units() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        systemd: false,
        ..Default::default()
    });
    let report = manager.show().unwrap();
    assert_eq!(report.file, UnitFileStatus::Missing);
    assert!(report.state.is_err());
    assert!(report.to_string().contains("State:        unavailable"));
    assert!(calls(&manager).is_empty());

    fs::write(env.unit_path(), "[Service]\nExecStart=/bin/true\n").unwrap();
    let report = manager.show().unwrap();
    assert_eq!(report.file, UnitFileStatus::NotManaged);
    assert!(
        report
            .to_string()
            .contains("not written by `mongoose setup`")
    );
}

#[test]
fn show_flags_pending_reload_and_conflicts() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        show_output:
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nNeedDaemonReload=yes\n".into(),
        ..Default::default()
    });
    fs::write(
        env.unit_path(),
        render_unit(&env.exe, &specs(&["*:1:1.1.1.1:1"])),
    )
    .unwrap();
    fs::create_dir(env.other_dir.join(DROP_IN_DIR_NAME)).unwrap();
    let text = manager.show().unwrap().to_string();
    assert!(text.contains("daemon-reload"), "{text}");
    assert!(text.contains("Conflicts:"), "{text}");
    assert!(text.contains(DROP_IN_DIR_NAME), "{text}");
}

#[test]
fn show_reports_corrupt_managed_unit() {
    let env = Env::new();
    fs::write(env.unit_path(), format!("{MARKER}\n[Service]\n")).unwrap();
    let report = env.manager(FakeSystem::default()).show().unwrap();
    assert!(matches!(
        report.file,
        UnitFileStatus::ManagedButInvalid { .. }
    ));
}
