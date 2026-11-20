use std::cell::RefCell;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use mongoose::service::{
    DROP_IN_DIR_NAME, MARKER, SHOW_PROPERTIES, ServiceError, ServiceManager, SetupOutcome, System,
    UNIT_NAME, UnitConflictKind, UnitFileStatus, UnitState, is_managed, parse_unit, quote_exec_arg,
    render_unit, split_exec_args,
};
use mongoose::spec::ForwardSpec;
use mongoose::sys::Storage;
use tempfile::TempDir;

struct FakeSystem {
    root: bool,
    systemd: bool,
    fail: Option<&'static str>,
    show_output: String,
    /// Answer to `systemctl show --property=ActiveState --value`.
    active_state: &'static str,
    calls: RefCell<Vec<String>>,
    /// Paths under these prefixes count as tmpfs.
    temporary: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
    immutable: Vec<PathBuf>,
    /// Use the real filesystem checks instead of the lists above.
    real_fs: bool,
}

impl Default for FakeSystem {
    fn default() -> Self {
        Self {
            root: true,
            systemd: true,
            fail: None,
            show_output: String::new(),
            active_state: "inactive",
            calls: RefCell::new(Vec::new()),
            temporary: Vec::new(),
            read_only: Vec::new(),
            immutable: Vec::new(),
            real_fs: false,
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

    fn ipv6_dual_stack(&self) -> bool {
        true
    }

    fn storage(&self, path: &Path) -> io::Result<Storage> {
        if self.real_fs {
            return mongoose::sys::storage(path);
        }
        Ok(Storage {
            temporary: self.temporary.iter().any(|p| path.starts_with(p)),
            read_only: self.read_only.iter().any(|p| path.starts_with(p)),
        })
    }

    fn is_immutable(&self, path: &Path) -> io::Result<bool> {
        if self.real_fs {
            return mongoose::sys::is_immutable(path);
        }
        Ok(self.immutable.iter().any(|p| p == path))
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
        Ok(if args.contains(&"--value") {
            format!("{}\n", self.active_state)
        } else if args[0] == "show" {
            self.show_output.clone()
        } else {
            String::new()
        })
    }
}

/// A fake systemd layout: `etc` (where new units go), then `run` (marked
/// temporary unless the real filesystem is used) and `lib`.
struct Env {
    _tmp: TempDir,
    etc: PathBuf,
    run: PathBuf,
    lib: PathBuf,
    exe: PathBuf,
}

impl Env {
    fn new() -> Self {
        Self::in_dir(TempDir::new().unwrap())
    }

    fn in_dir(tmp: TempDir) -> Self {
        let etc = tmp.path().join("etc");
        let run = tmp.path().join("run");
        let lib = tmp.path().join("lib");
        for dir in [&etc, &run, &lib] {
            fs::create_dir_all(dir).unwrap();
        }
        let exe = tmp.path().join("bin dir").join("mongoose");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        Self {
            _tmp: tmp,
            etc,
            run,
            lib,
            exe,
        }
    }

    fn manager(&self, mut system: FakeSystem) -> ServiceManager<FakeSystem> {
        system.temporary.push(self.run.clone());
        ServiceManager::new(system, &self.etc)
            .with_other_unit_dirs(vec![self.run.clone(), self.lib.clone()])
    }

    fn unit(&self, dir: &Path) -> PathBuf {
        dir.join(UNIT_NAME)
    }
}

fn specs(inputs: &[&str]) -> Vec<ForwardSpec> {
    inputs.iter().map(|s| s.parse().unwrap()).collect()
}

fn rules() -> Vec<ForwardSpec> {
    specs(&["*:80:10.0.0.1:80"])
}

fn calls(manager: &ServiceManager<FakeSystem>) -> Vec<String> {
    manager.system().calls.borrow().clone()
}

/// Writes a unit as `setup` would, without going through the manager.
fn install(env: &Env, dir: &Path) {
    fs::write(env.unit(dir), render_unit(&env.exe, &rules())).unwrap();
}

const FOREIGN: &str = "[Service]\nExecStart=/usr/bin/mongoose start tcp:*:81:10.0.0.1:81\n";
const STARTED: [&str; 3] = [
    "daemon-reload",
    "enable mongoose.service",
    "restart mongoose.service",
];
const ACTIVE_STATE: &str = "show mongoose.service --property=ActiveState --value";
const REMOVED: [&str; 3] = [
    "stop mongoose.service",
    "disable mongoose.service",
    "daemon-reload",
];

// ---------------------------------------------------------------------------
// Unit file text

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

// ---------------------------------------------------------------------------
// setup

#[test]
fn setup_installs_enables_and_starts() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    let plan = manager.setup(&env.exe, &rules(), false, false).unwrap();
    assert_eq!(plan.outcome, SetupOutcome::Installed);
    assert_eq!(plan.path, env.unit(&env.etc));

    let contents = fs::read_to_string(&plan.path).unwrap();
    assert_eq!(contents, render_unit(&env.exe, &rules()));
    let mode = fs::metadata(&plan.path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644);
    assert_eq!(calls(&manager), STARTED);
    // No temporary files are left behind.
    assert_eq!(fs::read_dir(&env.etc).unwrap().count(), 1);
}

#[test]
fn setup_skip_enable_only_writes_when_not_running() {
    for state in ["inactive", "failed", "deactivating"] {
        let env = Env::new();
        install(&env, &env.etc);
        let manager = env.manager(FakeSystem {
            active_state: state,
            ..Default::default()
        });
        manager
            .setup(&env.exe, &specs(&["*:81:10.0.0.1:81"]), true, false)
            .unwrap();
        assert!(env.unit(&env.etc).is_file());
        assert_eq!(calls(&manager), vec![ACTIVE_STATE], "{state}");
    }
}

#[test]
fn setup_skip_enable_never_restarts_a_running_service() {
    for state in ["active", "activating", "reloading"] {
        let env = Env::new();
        install(&env, &env.etc);
        let manager = env.manager(FakeSystem {
            active_state: state,
            ..Default::default()
        });
        let new_rules = specs(&["*:81:10.0.0.1:81"]);
        manager.setup(&env.exe, &new_rules, true, false).unwrap();
        // Only the read-only state query, for the log hint.
        assert_eq!(calls(&manager), vec![ACTIVE_STATE], "{state}");
        assert_eq!(
            fs::read_to_string(env.unit(&env.etc)).unwrap(),
            render_unit(&env.exe, &new_rules)
        );
    }
}

#[test]
fn setup_always_restarts_without_skip_enable() {
    for state in ["active", "inactive"] {
        let env = Env::new();
        install(&env, &env.etc);
        let manager = env.manager(FakeSystem {
            active_state: state,
            ..Default::default()
        });
        manager.setup(&env.exe, &rules(), false, false).unwrap();
        assert_eq!(calls(&manager), STARTED, "{state}");
    }
}

#[test]
fn setup_replaces_own_unit() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    install(&env, &env.etc);
    let new_rules = specs(&["*:81:10.0.0.1:81"]);
    let plan = manager.setup(&env.exe, &new_rules, false, false).unwrap();
    assert_eq!(plan.outcome, SetupOutcome::Replaced);
    assert_eq!(
        fs::read_to_string(env.unit(&env.etc)).unwrap(),
        render_unit(&env.exe, &new_rules)
    );
    assert_eq!(calls(&manager), STARTED);
}

#[test]
fn own_unit_outside_default_dir_is_replaced_in_place() {
    let env = Env::new();
    fs::write(env.unit(&env.lib), render_unit(&env.exe, &rules())).unwrap();
    let manager = env.manager(FakeSystem::default());
    let new_rules = specs(&["*:81:10.0.0.1:81"]);
    let plan = manager.setup(&env.exe, &new_rules, false, false).unwrap();
    assert_eq!(plan.outcome, SetupOutcome::Replaced);
    assert_eq!(plan.path, env.unit(&env.lib));
    assert_eq!(
        fs::read_to_string(env.unit(&env.lib)).unwrap(),
        render_unit(&env.exe, &new_rules)
    );
    assert!(!env.unit(&env.etc).exists());
}

#[test]
fn setup_refuses_foreign_unit_without_force() {
    for dir in ["etc", "lib"] {
        let env = Env::new();
        let path = env.unit(&if dir == "etc" {
            env.etc.clone()
        } else {
            env.lib.clone()
        });
        fs::write(&path, FOREIGN).unwrap();
        let manager = env.manager(FakeSystem::default());
        let err = manager.setup(&env.exe, &rules(), false, false).unwrap_err();
        assert!(
            matches!(&err, ServiceError::NotManaged(p) if *p == path),
            "{err}"
        );
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), FOREIGN);
        assert!(calls(&manager).is_empty());
    }
}

#[test]
fn marker_must_be_the_first_line() {
    let env = Env::new();
    fs::write(env.unit(&env.etc), format!("[Unit]\n{MARKER}\n")).unwrap();
    let err = env
        .manager(FakeSystem::default())
        .setup(&env.exe, &rules(), true, false)
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotManaged(_)));
}

#[test]
fn force_replaces_foreign_unit_in_place() {
    let env = Env::new();
    fs::write(env.unit(&env.lib), FOREIGN).unwrap();
    let manager = env.manager(FakeSystem::default());
    let plan = manager.setup(&env.exe, &rules(), false, true).unwrap();
    assert_eq!(plan.outcome, SetupOutcome::ReplacedForeign);
    assert_eq!(plan.path, env.unit(&env.lib));
    assert!(
        !env.unit(&env.etc).exists(),
        "must not add a shadowing unit"
    );
    assert_eq!(
        fs::read_to_string(env.unit(&env.lib)).unwrap(),
        render_unit(&env.exe, &rules())
    );
    assert_eq!(calls(&manager), STARTED);

    // From now on it is a unit written by setup, wherever it lives.
    let plan = manager.setup(&env.exe, &rules(), true, false).unwrap();
    assert_eq!(plan.outcome, SetupOutcome::Replaced);
    assert_eq!(manager.remove(false).unwrap(), env.unit(&env.lib));
    assert!(!env.unit(&env.lib).exists());
}

#[test]
fn force_is_not_needed_without_an_existing_unit() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    let plan = manager.setup(&env.exe, &rules(), true, true).unwrap();
    assert_eq!(plan.outcome, SetupOutcome::Installed);
    assert_eq!(plan.path, env.unit(&env.etc));
}

/// Expected refusal reason and a function creating the unit.
type Case = (&'static str, Box<dyn Fn()>);

/// Units that are not clearly removable, created in `dir`.
fn unqualified_units(env: &Env, dir: &Path) -> Vec<Case> {
    let path = env.unit(dir);
    let other = env.lib.parent().unwrap().join("elsewhere.service");
    let p = path.clone();
    let mut cases: Vec<Case> = vec![
        (
            "is a symlink",
            Box::new({
                let p = p.clone();
                move || std::os::unix::fs::symlink("/dev/null", &p).unwrap()
            }),
        ),
        (
            "is a symlink",
            Box::new({
                let (p, other) = (p.clone(), other.clone());
                move || {
                    fs::write(&other, FOREIGN).unwrap();
                    std::os::unix::fs::symlink(&other, &p).unwrap();
                }
            }),
        ),
        (
            "is not a regular file",
            Box::new({
                let p = p.clone();
                move || fs::create_dir(&p).unwrap()
            }),
        ),
        (
            "has 2 hard links",
            Box::new({
                let (p, other) = (p.clone(), other.clone());
                move || {
                    fs::write(&other, FOREIGN).unwrap();
                    fs::hard_link(&other, &p).unwrap();
                }
            }),
        ),
        (
            "is not valid UTF-8",
            Box::new({
                let p = p.clone();
                move || fs::write(&p, b"[Service]\n\xff\xfe\n").unwrap()
            }),
        ),
        (
            "is larger than",
            Box::new({
                let p = p.clone();
                move || fs::write(&p, vec![b'#'; 1024 * 1024 + 1]).unwrap()
            }),
        ),
    ];
    cases.push((
        "is not a regular file",
        Box::new(move || {
            let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            // SAFETY: mkfifo with a valid path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        }),
    ));
    cases
}

#[test]
fn force_only_applies_to_clearly_removable_units() {
    for dir_name in ["etc", "lib"] {
        let probe = Env::new();
        let count = unqualified_units(&probe, &probe.etc).len();
        for index in 0..count {
            let env = Env::new();
            let dir = if dir_name == "etc" {
                env.etc.clone()
            } else {
                env.lib.clone()
            };
            let (reason, create) = unqualified_units(&env, &dir).swap_remove(index);
            create();
            let manager = env.manager(FakeSystem::default());
            for result in [
                manager.setup(&env.exe, &rules(), false, true).map(|_| ()),
                manager.remove(true).map(|_| ()),
            ] {
                let err = result.unwrap_err();
                assert!(
                    matches!(err, ServiceError::ForceNotAllowed { .. }),
                    "{dir_name}/{reason}: {err}"
                );
                assert!(err.to_string().contains(reason), "{err}");
            }
            assert!(calls(&manager).is_empty());
            assert!(!env.unit(&env.etc).is_file() || dir_name == "etc");
        }
    }
}

#[test]
fn force_never_touches_temporary_storage() {
    let env = Env::new();
    fs::write(env.unit(&env.run), FOREIGN).unwrap();
    let manager = env.manager(FakeSystem::default());
    for result in [
        manager.setup(&env.exe, &rules(), false, true).map(|_| ()),
        manager.remove(true).map(|_| ()),
    ] {
        let err = result.unwrap_err();
        assert!(matches!(err, ServiceError::Unmodifiable { .. }), "{err}");
        assert!(err.to_string().contains("temporary storage"), "{err}");
    }
    assert_eq!(fs::read_to_string(env.unit(&env.run)).unwrap(), FOREIGN);
    assert!(!env.unit(&env.etc).exists());
    assert!(calls(&manager).is_empty());
}

#[test]
fn own_unit_on_temporary_storage_is_not_replaced_either() {
    let env = Env::new();
    fs::write(env.unit(&env.run), render_unit(&env.exe, &rules())).unwrap();
    let err = env
        .manager(FakeSystem::default())
        .setup(&env.exe, &rules(), false, false)
        .unwrap_err();
    assert!(matches!(err, ServiceError::Unmodifiable { .. }), "{err}");
}

#[test]
fn setup_refuses_a_default_dir_on_temporary_storage() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        temporary: vec![env.etc.clone()],
        ..Default::default()
    });
    let err = manager.setup(&env.exe, &rules(), false, false).unwrap_err();
    assert!(
        err.to_string().contains("would not survive a reboot"),
        "{err}"
    );
    assert!(!env.unit(&env.etc).exists());
}

#[test]
fn real_tmpfs_is_detected() {
    let shm = Path::new("/dev/shm");
    if !mongoose::sys::storage(shm).is_ok_and(|s| s.temporary) {
        eprintln!("skipping: /dev/shm is not tmpfs here");
        return;
    }
    let env = Env::in_dir(TempDir::new_in(shm).unwrap());
    let manager = ServiceManager::new(
        FakeSystem {
            real_fs: true,
            ..Default::default()
        },
        &env.etc,
    );
    let err = manager.setup(&env.exe, &rules(), false, false).unwrap_err();
    assert!(err.to_string().contains("temporary storage"), "{err}");
}

#[test]
fn read_only_and_immutable_are_refused_before_anything_happens() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    manager.setup(&env.exe, &rules(), true, false).unwrap();
    let unit = env.unit(&env.etc);
    let cases = [
        FakeSystem {
            read_only: vec![env.etc.clone()],
            ..Default::default()
        },
        FakeSystem {
            immutable: vec![unit.clone()],
            ..Default::default()
        },
        FakeSystem {
            immutable: vec![env.etc.clone()],
            ..Default::default()
        },
    ];
    for system in cases {
        let manager = env.manager(system);
        for result in [
            manager.setup(&env.exe, &rules(), false, false).map(|_| ()),
            manager.remove(false).map(|_| ()),
        ] {
            assert!(matches!(result, Err(ServiceError::Unmodifiable { .. })));
        }
        // Not even `systemctl stop` ran.
        assert!(calls(&manager).is_empty());
        assert!(unit.is_file());
    }
}

#[test]
fn real_immutable_check_handles_plain_files() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("f");
    fs::write(&file, b"x").unwrap();
    assert!(!mongoose::sys::is_immutable(&file).unwrap());
    assert!(!mongoose::sys::is_immutable(tmp.path()).unwrap());
}

#[test]
fn several_unit_files_are_refused_even_with_force() {
    let env = Env::new();
    fs::write(env.unit(&env.etc), render_unit(&env.exe, &rules())).unwrap();
    fs::write(env.unit(&env.lib), FOREIGN).unwrap();
    let manager = env.manager(FakeSystem::default());
    for force in [false, true] {
        for result in [
            manager.setup(&env.exe, &rules(), false, force).map(|_| ()),
            manager.remove(force).map(|_| ()),
        ] {
            let Err(ServiceError::Conflicts(conflicts)) = result else {
                panic!("expected conflicts");
            };
            assert_eq!(conflicts.len(), 2);
            assert!(
                conflicts
                    .iter()
                    .all(|c| c.kind == UnitConflictKind::OtherUnit)
            );
        }
    }
    assert!(calls(&manager).is_empty());
}

#[test]
fn the_same_file_reached_twice_counts_once() {
    // Like /lib -> /usr/lib on merged-/usr systems.
    let env = Env::new();
    let alias = env.lib.parent().unwrap().join("lib-alias");
    std::os::unix::fs::symlink(&env.lib, &alias).unwrap();
    fs::write(env.unit(&env.lib), FOREIGN).unwrap();
    let manager = ServiceManager::new(FakeSystem::default(), &env.etc)
        .with_other_unit_dirs(vec![alias.clone(), env.lib.clone()]);
    assert_eq!(manager.find_units().len(), 1);
    let plan = manager.setup(&env.exe, &rules(), true, true).unwrap();
    assert_eq!(plan.path, alias.join(UNIT_NAME));
    assert!(is_managed(&fs::read_to_string(env.unit(&env.lib)).unwrap()));
}

#[test]
fn drop_ins_are_refused_even_with_force() {
    let env = Env::new();
    fs::create_dir(env.lib.join(DROP_IN_DIR_NAME)).unwrap();
    let manager = env.manager(FakeSystem::default());
    for force in [false, true] {
        let err = manager.setup(&env.exe, &rules(), false, force).unwrap_err();
        let ServiceError::Conflicts(conflicts) = &err else {
            panic!("unexpected {err}");
        };
        assert_eq!(conflicts[0].kind, UnitConflictKind::DropIn);
    }
    assert!(!env.unit(&env.etc).exists());
}

#[test]
fn setup_requires_root_and_systemd() {
    let env = Env::new();
    let not_root = env.manager(FakeSystem {
        root: false,
        ..Default::default()
    });
    assert!(matches!(
        not_root.setup(&env.exe, &rules(), false, true),
        Err(ServiceError::NotRoot)
    ));
    let no_systemd = env.manager(FakeSystem {
        systemd: false,
        ..Default::default()
    });
    assert!(matches!(
        no_systemd.setup(&env.exe, &rules(), false, true),
        Err(ServiceError::NoSystemd)
    ));
    assert!(!env.unit(&env.etc).exists());
}

#[test]
fn setup_requires_existing_absolute_executable() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    for exe in [Path::new("mongoose"), Path::new("/nonexistent/mongoose")] {
        assert!(matches!(
            manager.setup(exe, &rules(), false, false),
            Err(ServiceError::InvalidExecutable(_))
        ));
    }
}

#[test]
fn failure_after_writing_is_reported_without_rollback() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        fail: Some("enable"),
        ..Default::default()
    });
    let err = manager.setup(&env.exe, &rules(), false, false).unwrap_err();
    assert!(matches!(err, ServiceError::Systemctl { .. }));
    assert_eq!(
        calls(&manager),
        vec!["daemon-reload", "enable mongoose.service"]
    );
    assert_eq!(
        fs::read_to_string(env.unit(&env.etc)).unwrap(),
        render_unit(&env.exe, &rules())
    );
}

// ---------------------------------------------------------------------------
// remove

#[test]
fn remove_stops_disables_and_deletes() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    install(&env, &env.etc);
    assert_eq!(manager.remove(false).unwrap(), env.unit(&env.etc));
    assert!(!env.unit(&env.etc).exists());
    assert_eq!(calls(&manager), REMOVED);
}

#[test]
fn remove_requires_a_unit() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    for force in [false, true] {
        assert!(matches!(
            manager.remove(force),
            Err(ServiceError::NotInstalled)
        ));
    }
    assert!(calls(&manager).is_empty());
}

#[test]
fn remove_refuses_foreign_unit_without_force() {
    let env = Env::new();
    fs::write(env.unit(&env.etc), FOREIGN).unwrap();
    let manager = env.manager(FakeSystem::default());
    assert!(matches!(
        manager.remove(false),
        Err(ServiceError::NotManaged(_))
    ));
    assert!(env.unit(&env.etc).exists());
    assert!(calls(&manager).is_empty());
}

#[test]
fn force_removes_foreign_unit_wherever_it_is() {
    for in_lib in [false, true] {
        let env = Env::new();
        let path = env.unit(if in_lib { &env.lib } else { &env.etc });
        fs::write(&path, FOREIGN).unwrap();
        let manager = env.manager(FakeSystem::default());
        assert_eq!(manager.remove(true).unwrap(), path);
        assert!(!path.exists());
        assert_eq!(calls(&manager), REMOVED);
    }
}

#[test]
fn remove_leaves_drop_ins_in_place() {
    let env = Env::new();
    let manager = env.manager(FakeSystem::default());
    manager.setup(&env.exe, &rules(), true, false).unwrap();
    let drop_in = env.etc.join(DROP_IN_DIR_NAME);
    fs::create_dir(&drop_in).unwrap();
    manager.remove(false).unwrap();
    assert!(!env.unit(&env.etc).exists());
    assert!(drop_in.is_dir());
}

#[test]
fn remove_stops_on_systemctl_failure() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        fail: Some("stop"),
        ..Default::default()
    });
    manager.setup(&env.exe, &rules(), true, false).unwrap();
    assert!(manager.remove(false).is_err());
    assert!(env.unit(&env.etc).exists());
}

#[test]
fn remove_requires_root() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        root: false,
        ..Default::default()
    });
    assert!(matches!(manager.remove(true), Err(ServiceError::NotRoot)));
}

// ---------------------------------------------------------------------------
// show

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

fn show_text(env: &Env) -> (UnitFileStatus, String) {
    let report = env
        .manager(FakeSystem {
            root: false,
            ..Default::default()
        })
        .show()
        .unwrap();
    let text = report.to_string();
    (report.file, text)
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
    fs::write(env.unit(&env.etc), render_unit(&env.exe, &rules)).unwrap();

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
            "show mongoose.service --property={SHOW_PROPERTIES}"
        )]
    );

    let text = report.to_string();
    for needle in [
        "Managed:      yes",
        "  #1 tcp:*:80:10.0.0.1:8080",
        "tcp 0.0.0.0:80, [::]:80 -> 10.0.0.1:8080 (1 port, 2 sockets)",
        "Active:       active (running) since Fri 2026-11-20",
        "Enabled:      enabled",
        "Main PID:     4242",
        "journalctl -u mongoose.service",
        "systemctl status mongoose.service",
    ] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
    assert!(!text.contains("Warning"), "{text}");
}

#[test]
fn show_missing_unit() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        systemd: false,
        ..Default::default()
    });
    let report = manager.show().unwrap();
    assert_eq!(report.file, UnitFileStatus::Missing);
    assert_eq!(report.path, env.unit(&env.etc));
    let text = report.to_string();
    assert!(text.contains("none found (setup would create"), "{text}");
    assert!(text.contains("State:        unavailable"), "{text}");
    assert!(calls(&manager).is_empty());
}

#[test]
fn show_interprets_foreign_units() {
    let env = Env::new();
    fs::write(
        env.unit(&env.lib),
        "# hand written\n[Service]\nExecStart = -/usr/bin/mongoose -v start \\\n  \
         tcp:[::]:80:10.0.0.1:80\n",
    )
    .unwrap();
    let (file, text) = show_text(&env);
    let UnitFileStatus::Foreign {
        link_target,
        parsed,
    } = file
    else {
        panic!("{text}");
    };
    assert_eq!(link_target, None);
    assert_eq!(parsed.unwrap().specs, specs(&["tcp:[::]:80:10.0.0.1:80"]));
    for needle in [
        &format!("Unit file:    {}", env.unit(&env.lib).display()),
        "Managed:      no (not written by `mongoose setup`)",
        "Executable:   /usr/bin/mongoose",
        "Verbose:      yes",
        "Note:         ExecStart '-' prefix",
        "Rules (best-effort interpretation):",
        "tcp [::]:80 (dual-stack, also IPv4 per system default) -> 10.0.0.1:80",
    ] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
}

#[test]
fn show_reports_uninterpretable_foreign_units() {
    let cases = [
        (
            "[Service]\nExecStart=/usr/bin/socat TCP-LISTEN:80 TCP:x:80\n",
            "not a valid mongoose command line",
        ),
        (
            "[Service]\nExecStart=%h/mongoose start tcp:*:80:10.0.0.1:80\n",
            "specifier %h",
        ),
        ("[Unit]\nDescription=nothing to run\n", "no ExecStart="),
        ("", "no ExecStart="),
    ];
    for (contents, needle) in cases {
        let env = Env::new();
        fs::write(env.unit(&env.etc), contents).unwrap();
        let (file, text) = show_text(&env);
        assert!(
            matches!(file, UnitFileStatus::Foreign { parsed: Err(_), .. }),
            "{text}"
        );
        assert!(
            text.contains("Rules:        cannot interpret the unit: "),
            "{text}"
        );
        assert!(text.contains(needle), "{needle}\n{text}");
    }
}

#[test]
fn show_warns_about_colliding_rules() {
    let env = Env::new();
    fs::write(
        env.unit(&env.etc),
        "[Service]\nExecStart=/usr/bin/mongoose start tcp:*:80:10.0.0.1:80 tcp:0.0.0.0:80:10.0.0.2:80\n",
    )
    .unwrap();
    let (_, text) = show_text(&env);
    assert!(text.contains("Warning:      these rules collide"), "{text}");
    assert!(
        text.contains("rule #1 'tcp:*:80:10.0.0.1:80' and rule #2"),
        "{text}"
    );
}

#[test]
fn show_handles_unusual_files_without_blocking() {
    type Make = fn(&Path, &Path);
    let cases: [(&str, Make, &str); 6] = [
        (
            "masked",
            |p, _| std::os::unix::fs::symlink("/dev/null", p).unwrap(),
            "masked",
        ),
        (
            "dangling symlink",
            |p, base| std::os::unix::fs::symlink(base.join("missing"), p).unwrap(),
            "cannot read",
        ),
        (
            "directory",
            |p, _| fs::create_dir(p).unwrap(),
            "is a directory",
        ),
        (
            "fifo",
            |p, _| {
                let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
                // SAFETY: mkfifo with a valid path.
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
            },
            "is not a regular file",
        ),
        (
            "binary",
            |p, _| fs::write(p, b"\xff\xfe\x00").unwrap(),
            "not valid UTF-8",
        ),
        (
            "huge",
            |p, _| fs::write(p, vec![b'#'; 1024 * 1024 + 1]).unwrap(),
            "larger than",
        ),
    ];
    for (name, make, needle) in cases {
        let env = Env::new();
        make(&env.unit(&env.etc), env.lib.parent().unwrap());
        let (_, text) = show_text(&env);
        assert!(text.contains(needle), "{name}: {needle}\n{text}");
    }
}

#[test]
fn show_follows_symlinks_to_unit_files() {
    let env = Env::new();
    let target = env.lib.parent().unwrap().join("real.service");
    fs::write(&target, render_unit(&env.exe, &rules())).unwrap();
    std::os::unix::fs::symlink(&target, env.unit(&env.etc)).unwrap();
    let (file, text) = show_text(&env);
    // Units written by setup are never symlinks, so this is interpreted as
    // a foreign unit, through the link.
    let UnitFileStatus::Foreign {
        link_target,
        parsed,
    } = file
    else {
        panic!("{text}");
    };
    assert_eq!(link_target, Some(target.clone()));
    assert_eq!(parsed.unwrap().specs, rules());
    assert!(
        text.contains(&format!("Symlink to:   {}", target.display())),
        "{text}"
    );
}

#[test]
fn show_lists_shadowed_units_and_drop_ins() {
    let env = Env::new();
    fs::write(env.unit(&env.etc), render_unit(&env.exe, &rules())).unwrap();
    fs::write(env.unit(&env.lib), FOREIGN).unwrap();
    fs::create_dir(env.run.join(DROP_IN_DIR_NAME)).unwrap();
    let report = env.manager(FakeSystem::default()).show().unwrap();
    // The unit systemd loads is the one in the highest priority directory.
    assert_eq!(report.path, env.unit(&env.etc));
    assert!(matches!(report.file, UnitFileStatus::Managed { .. }));
    assert_eq!(report.conflicts.len(), 2);
    let text = report.to_string();
    assert!(
        text.contains("one of several mongoose.service files"),
        "{text}"
    );
    assert!(text.contains("drop-in directory"), "{text}");
}

#[test]
fn show_flags_pending_reload() {
    let env = Env::new();
    let manager = env.manager(FakeSystem {
        show_output:
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nNeedDaemonReload=yes\n".into(),
        ..Default::default()
    });
    fs::write(env.unit(&env.etc), render_unit(&env.exe, &rules())).unwrap();
    let text = manager.show().unwrap().to_string();
    assert!(text.contains("daemon-reload"), "{text}");
}

#[test]
fn show_reports_corrupt_managed_unit() {
    let env = Env::new();
    fs::write(env.unit(&env.etc), format!("{MARKER}\n[Service]\n")).unwrap();
    let (file, _) = show_text(&env);
    assert!(matches!(file, UnitFileStatus::ManagedButInvalid { .. }));
}
