#![cfg(unix)]

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions, Permissions},
    io::Write,
    net::{SocketAddr, TcpListener},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use urma_node::{
    install::{Action, Error, Host, Init, InitChoice, Outcome, Registration, Runner, System},
    platform::ServiceSettings,
};

struct Fake {
    root: PathBuf,
    uid: u32,
    gid: u32,
    calls: Vec<String>,
    answers: HashMap<String, VecDeque<Outcome>>,
}

fn ok(stdout: &str) -> Outcome {
    Outcome {
        success: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

impl Fake {
    fn append(&self, file: &str, line: &str) -> Result<(), Error> {
        let mut table = OpenOptions::new().append(true).open(self.root.join(file))?;
        writeln!(table, "{line}")?;
        Ok(())
    }

    fn create_account(&self) -> Result<(), Error> {
        self.append(
            "etc/passwd",
            &format!(
                "urma-node:x:{}:{}::/var/lib/urma-node:/usr/sbin/nologin",
                self.uid, self.gid
            ),
        )?;
        let group = fs::read_to_string(self.root.join("etc/group"))?;
        match group.contains("urma-node:") {
            true => Ok(()),
            false => self.append("etc/group", &format!("urma-node:x:{}:", self.gid)),
        }
    }

    fn add_member(&self, user: &str) -> Result<(), Error> {
        let path = self.root.join("etc/group");
        let group = fs::read_to_string(&path)?;
        let prefix = format!("urma-node:x:{}:", self.gid);
        let updated = group.replace(&format!("{prefix}\n"), &format!("{prefix}{user}\n"));
        fs::write(path, updated)?;
        Ok(())
    }
}

impl Fake {
    fn answer(&mut self, call: &str, stdouts: &[&str]) {
        let queue = stdouts.iter().map(|stdout| ok(stdout)).collect();
        self.answers.insert(call.to_owned(), queue);
    }
}

impl Runner for Fake {
    fn run(&mut self, program: &str, args: &[&str]) -> Result<Outcome, Error> {
        assert!(
            program.starts_with('/'),
            "{program} is not an absolute path"
        );
        let call = [&[program], args].concat().join(" ");
        self.calls.push(call.clone());
        let name = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::Failed(format!("bad program {program}")))?;
        match (name, args.first().copied()) {
            ("useradd", ..) | ("adduser", Some("-S")) => self.create_account()?,
            ("addgroup", Some("-S")) => {
                self.append("etc/group", &format!("urma-node:x:{}:", self.gid))?
            }
            ("usermod", ..) => self.add_member(args[3])?,
            ("adduser", Some(user)) => self.add_member(user)?,
            _ => {}
        }
        match self.answers.get_mut(&call) {
            Some(queue) if queue.len() > 1 => {
                Ok(queue.pop_front().ok_or_else(|| Error::Failed(call))?)
            }
            Some(queue) => Ok(queue.front().cloned().ok_or_else(|| Error::Failed(call))?),
            None => Ok(ok("")),
        }
    }
}

struct Lab {
    dir: TempDir,
}

impl Lab {
    fn new(markers: &[&str], programs: &[&str]) -> Result<Self, Error> {
        let dir = TempDir::new()?;
        let root = dir.path();
        fs::create_dir_all(root.join("etc"))?;
        fs::write(
            root.join("etc/passwd"),
            "root:x:0:0::/root:/bin/sh\nalice:x:1000:1000::/home/alice:/bin/sh\n",
        )?;
        fs::write(root.join("etc/group"), "root:x:0:\nalice:x:1000:\n")?;
        for marker in markers {
            fs::create_dir_all(root.join(marker))?;
        }
        fs::create_dir_all(root.join("usr/bin"))?;
        fs::create_dir_all(root.join("usr/sbin"))?;
        fs::write(root.join("usr/sbin/nologin"), "")?;
        for program in programs {
            fs::write(root.join("usr/bin").join(program), "")?;
        }
        let binary = root.join("usr/bin/urma-node");
        fs::write(&binary, "")?;
        fs::set_permissions(&binary, Permissions::from_mode(0o755))?;
        Ok(Self { dir })
    }

    fn system(&self, sudo: (&str, &str), pkexec_uid: &str) -> Result<System<Fake>, Error> {
        let root = self.dir.path().to_path_buf();
        let meta = fs::metadata(&root)?;
        Ok(System {
            root: root.clone(),
            host: Host {
                sudo_user: sudo.0.to_owned(),
                sudo_uid: sudo.1.to_owned(),
                pkexec_uid: pkexec_uid.to_owned(),
                binary: PathBuf::from("/usr/bin/urma-node"),
            },
            runner: Fake {
                root,
                uid: meta.uid(),
                gid: meta.gid(),
                calls: Vec::new(),
                answers: HashMap::new(),
            },
        })
    }

    fn path(&self, logical: &str) -> PathBuf {
        self.dir.path().join(logical)
    }

    fn mode(&self, logical: &str) -> Result<u32, Error> {
        Ok(fs::metadata(self.path(logical))?.permissions().mode() & 0o7777)
    }
}

const NOBODY: (&str, &str) = ("", "");
const ALICE: (&str, &str) = ("alice", "1000");
const SYSTEMD: [&str; 5] = ["useradd", "usermod", "userdel", "groupdel", "systemctl"];
const OPENRC: [&str; 6] = [
    "adduser",
    "addgroup",
    "deluser",
    "delgroup",
    "rc-update",
    "rc-service",
];
const RUNIT: [&str; 3] = ["sv", "useradd", "usermod"];

fn install(system: &mut System<Fake>) -> Result<String, Error> {
    let mut out = Vec::new();
    system.execute(
        Action::Install {
            init: InitChoice::Auto,
        },
        &mut out,
    )?;
    String::from_utf8(out).map_err(|error| Error::Failed(error.to_string()))
}

fn status(system: &mut System<Fake>) -> Result<String, Error> {
    let mut out = Vec::new();
    system.execute(Action::Status, &mut out)?;
    String::from_utf8(out).map_err(|error| Error::Failed(error.to_string()))
}

fn snapshot(name: &str) -> Result<String, Error> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/install")
        .join(name);
    Ok(fs::read_to_string(path)?)
}

fn point_control_at(lab: &Lab, address: SocketAddr) -> Result<(), Error> {
    let path = lab.path("etc/urma-node/service.json");
    let mut settings = ServiceSettings::read(&path)?;
    settings.listen = address;
    fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(())
}

fn closed_port() -> Result<SocketAddr, Error> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

const HINT: &str = "To read the node token as your desktop user, run: sudo usermod -a -G urma-node <user>, then log in again.\n";

#[test]
fn detection_follows_the_contract_order() -> Result<(), Error> {
    let cases: [(&[&str], &[&str], Init); 5] = [
        (
            &["run/systemd/system", "run/openrc"],
            &["openrc-run", "sv"],
            Init::Systemd,
        ),
        (&["run/openrc"], &[], Init::Openrc),
        (&[], &["openrc-run"], Init::Openrc),
        (&["run/runit"], &["sv"], Init::Runit),
        (&["etc/runit/runsvdir"], &["sv"], Init::Runit),
    ];
    for (markers, programs, expected) in cases {
        let lab = Lab::new(markers, programs)?;
        assert_eq!(
            lab.system(NOBODY, "")?.detect()?,
            expected,
            "{markers:?} {programs:?}"
        );
    }
    Ok(())
}

#[test]
fn an_unknown_init_names_the_manual_route_and_the_paths() -> Result<(), Error> {
    for (markers, programs) in [(&[][..], &[][..]), (&["run/runit"][..], &[][..])] {
        let lab = Lab::new(markers, programs)?;
        let mut system = lab.system(NOBODY, "")?;
        let error = match install(&mut system) {
            Err(error) => error,
            Ok(out) => panic!("install without an init system succeeded: {out}"),
        };
        assert!(matches!(error, Error::NoInit));
        let message = error.to_string();
        assert!(message.starts_with(
            "No supported init system found (systemd, OpenRC, runit). Configure your service \
             manager to run `urma-node service run` as user urma-node"
        ));
        for path in [
            "/usr/bin/urma-node",
            "/etc/urma-node/service.json",
            "/etc/urma-node/config.json",
            "/var/lib/urma-node/data",
            "/var/lib/urma-node/service.token",
            "/var/lib/urma-node/peer.key",
        ] {
            assert!(message.contains(path), "{path} missing from: {message}");
        }
        assert!(system.runner.calls.is_empty());
    }
    Ok(())
}

#[test]
fn adapters_render_the_snapshots() -> Result<(), Error> {
    let lab = Lab::new(&[], &[])?;
    let system = lab.system(NOBODY, "")?;
    let cases = [
        (
            Init::Systemd,
            vec![(
                "/etc/systemd/system/urma-node.service",
                "urma-node.service",
                0o644,
            )],
        ),
        (
            Init::Openrc,
            vec![("/etc/init.d/urma-node", "urma-node.openrc", 0o755)],
        ),
        (
            Init::Runit,
            vec![
                ("/etc/sv/urma-node/run", "runit-run", 0o755),
                ("/etc/sv/urma-node/log/run", "runit-log-run", 0o755),
            ],
        ),
    ];
    for (init, expected) in cases {
        let files = system.render(init)?;
        assert_eq!(files.len(), expected.len());
        for (file, (path, name, mode)) in files.iter().zip(expected) {
            assert_eq!(file.path, PathBuf::from(path));
            assert_eq!(file.content, snapshot(name)?, "{name}");
            assert_eq!(file.mode, mode);
        }
    }
    assert!(matches!(
        system.render(Init::Windows),
        Err(Error::Failed(..))
    ));
    Ok(())
}

#[test]
fn openrc_hands_the_service_only_its_log_file() -> Result<(), Error> {
    let lab = Lab::new(&[], &[])?;
    let files = lab.system(NOBODY, "")?.render(Init::Openrc)?;
    let script = &files[0].content;
    let log = "/var/log/urma-node/urma-node.log";
    assert!(script.contains(&format!("output_log=\"{log}\"")));
    assert!(script.contains(&format!("error_log=\"{log}\"")));
    assert!(script.contains(&format!(
        "start_pre() {{\n\tcheckpath -f -m 0640 -o urma-node:urma-node \"{log}\"\n}}"
    )));
    Ok(())
}

#[test]
fn systemd_install_creates_the_account_and_starts_once() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
    let mut system = lab.system(ALICE, "")?;
    let out = install(&mut system)?;
    assert_eq!(
        system.runner.calls,
        [
            "/usr/bin/useradd --system --home-dir /var/lib/urma-node --no-create-home --shell /usr/sbin/nologin --user-group urma-node",
            "/usr/bin/systemctl daemon-reload",
            "/usr/bin/systemctl enable --now urma-node.service",
            "/usr/bin/usermod -a -G urma-node alice",
        ]
    );
    assert_eq!(
        out,
        "Added alice to group urma-node; log in again to read the node token.\n"
    );
    assert_eq!(
        fs::read_to_string(lab.path("etc/systemd/system/urma-node.service"))?,
        snapshot("urma-node.service")?
    );
    assert_eq!(lab.mode("etc/systemd/system/urma-node.service")?, 0o644);
    assert_eq!(lab.mode("etc/urma-node")?, 0o755);
    assert_eq!(lab.mode("etc/urma-node/config.json")?, 0o644);
    assert_eq!(lab.mode("etc/urma-node/service.json")?, 0o644);
    assert_eq!(lab.mode("var/lib/urma-node")?, 0o750);
    assert_eq!(lab.mode("var/lib/urma-node/data")?, 0o700);
    assert_eq!(lab.mode("var/lib/urma-node/service.token")?, 0o640);
    assert_eq!(lab.mode("var/lib/urma-node/peer.key")?, 0o600);
    let token = fs::metadata(lab.path("var/lib/urma-node/service.token"))?;
    assert_eq!(
        (token.uid(), token.gid()),
        (system.runner.uid, system.runner.gid)
    );
    assert!(!lab.path("var/log/urma-node").exists());
    assert_eq!(
        system.registration()?,
        Registration::Registered(Init::Systemd)
    );
    Ok(())
}

#[test]
fn a_second_install_is_an_update() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
    let mut system = lab.system(ALICE, "")?;
    install(&mut system)?;
    let unit = fs::read_to_string(lab.path("etc/systemd/system/urma-node.service"))?;
    system.runner.calls.clear();
    let out = install(&mut system)?;
    assert_eq!(
        system.runner.calls,
        [
            "/usr/bin/systemctl daemon-reload",
            "/usr/bin/systemctl restart urma-node.service"
        ]
    );
    assert_eq!(out, "");
    assert_eq!(
        fs::read_to_string(lab.path("etc/systemd/system/urma-node.service"))?,
        unit
    );
    system.runner.calls.clear();
    system.execute(Action::Update, &mut Vec::new())?;
    assert_eq!(
        system.runner.calls,
        [
            "/usr/bin/systemctl daemon-reload",
            "/usr/bin/systemctl restart urma-node.service"
        ]
    );
    Ok(())
}

#[test]
fn pre_existing_state_is_never_chowned() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
    let outside = lab.path("etc/shadow");
    fs::write(&outside, "root:secret\n")?;
    fs::set_permissions(&outside, Permissions::from_mode(0o600))?;
    let before = fs::metadata(&outside)?;
    fs::create_dir_all(lab.path("etc/urma-node"))?;
    fs::create_dir_all(lab.path("var/lib/urma-node/data"))?;
    fs::write(lab.path("etc/urma-node/config.json"), "{}")?;
    fs::write(lab.path("etc/urma-node/service.json"), "{}")?;
    fs::write(lab.path("var/lib/urma-node/peer.key"), "key")?;
    fs::hard_link(&outside, lab.path("var/lib/urma-node/service.token"))?;
    let mut system = lab.system(NOBODY, "")?;
    system.runner.gid = 0;
    install(&mut system)?;
    let after = fs::metadata(&outside)?;
    assert_eq!(
        (after.uid(), after.gid(), after.mode()),
        (before.uid(), before.gid(), before.mode())
    );
    let state = fs::metadata(lab.path("var/lib/urma-node"))?;
    assert_eq!(state.gid(), before.gid());
    Ok(())
}

#[test]
fn a_symlinked_log_dir_is_refused_and_never_chmodded() -> Result<(), Error> {
    let lab = Lab::new(&["run/openrc"], &OPENRC)?;
    let target = lab.path("target");
    fs::create_dir_all(&target)?;
    fs::set_permissions(&target, Permissions::from_mode(0o700))?;
    fs::create_dir_all(lab.path("var/log"))?;
    symlink(&target, lab.path("var/log/urma-node"))?;
    let mut system = lab.system(NOBODY, "")?;
    let result = install(&mut system);
    assert!(matches!(result, Err(Error::Failed(..))), "{result:?}");
    assert_eq!(lab.mode("target")?, 0o700);
    assert!(lab.path("var/log/urma-node").is_symlink());
    assert!(!lab.path("etc/init.d/urma-node").exists());
    Ok(())
}

#[test]
fn a_binary_outside_usr_bin_or_writable_by_others_is_refused() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
    let mut system = lab.system(NOBODY, "")?;
    system.host.binary = PathBuf::from("/home/alice/urma-node");
    assert!(matches!(install(&mut system), Err(Error::Failed(..))));
    system.host.binary = PathBuf::from("/usr/bin/urma-node");
    fs::set_permissions(lab.path("usr/bin/urma-node"), Permissions::from_mode(0o775))?;
    assert!(matches!(install(&mut system), Err(Error::Failed(..))));
    assert!(system.runner.calls.is_empty());
    assert!(!lab.path("etc/urma-node").exists());
    Ok(())
}

#[test]
fn the_windows_adapter_is_refused_before_any_change() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
    let mut system = lab.system(ALICE, "")?;
    let init = InitChoice::Windows;
    let result = system.execute(Action::Install { init }, &mut Vec::new());
    assert!(matches!(result, Err(Error::Failed(..))));
    assert!(system.runner.calls.is_empty());
    assert!(!lab.path("etc/urma-node").exists());
    Ok(())
}

#[test]
fn update_without_a_registration_fails() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
    let mut system = lab.system(NOBODY, "")?;
    let result = system.execute(Action::Update, &mut Vec::new());
    assert!(matches!(result, Err(Error::NotInstalled)));
    assert!(system.runner.calls.is_empty());
    Ok(())
}

#[test]
fn status_reports_json() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
    let mut system = lab.system(NOBODY, "")?;
    let absent: serde_json::Value = serde_json::from_str(&status(&mut system)?)?;
    assert_eq!(absent["init"], "systemd");
    assert_eq!(absent["registered"], false);
    assert_eq!(absent["enabled"], false);
    assert_eq!(absent["running"], false);
    assert!(absent["listening"].is_boolean());
    install(&mut system)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    point_control_at(&lab, listener.local_addr()?)?;
    let answers = [
        (
            "/usr/bin/systemctl is-enabled urma-node.service",
            "enabled\n",
        ),
        ("/usr/bin/systemctl is-active urma-node.service", "active\n"),
    ];
    for (call, stdout) in answers {
        system.runner.answer(call, &[stdout]);
    }
    assert_eq!(
        status(&mut system)?,
        "{\"init\":\"systemd\",\"registered\":true,\"enabled\":true,\"running\":true,\"listening\":true}\n"
    );
    drop(listener);
    assert_eq!(
        status(&mut system)?,
        "{\"init\":\"systemd\",\"registered\":true,\"enabled\":true,\"running\":true,\"listening\":false}\n",
        "a supervisor that respawns a dead child is running but not listening"
    );
    Ok(())
}

#[test]
fn only_a_verified_invoker_joins_the_group() -> Result<(), Error> {
    let cases = [
        (("root", "0"), "", false),
        (("alice", "1001"), "", false),
        (("alice", ""), "", false),
        (("mallory", "1000"), "", false),
        (ALICE, "", true),
        (("bob", "1002"), "1000", true),
    ];
    for (sudo, pkexec_uid, joins) in cases {
        let lab = Lab::new(&["run/systemd/system"], &SYSTEMD)?;
        let mut system = lab.system(sudo, pkexec_uid)?;
        let out = install(&mut system)?;
        let added = system
            .runner
            .calls
            .iter()
            .any(|call| call == "/usr/bin/usermod -a -G urma-node alice");
        assert_eq!(added, joins, "{sudo:?} {pkexec_uid:?}");
        if !joins {
            assert_eq!(out, HINT);
            assert!(
                !system
                    .runner
                    .calls
                    .iter()
                    .any(|call| call.contains("usermod"))
            );
        }
    }
    Ok(())
}

#[test]
fn openrc_install_and_purge_with_busybox() -> Result<(), Error> {
    let lab = Lab::new(&["run/openrc"], &OPENRC)?;
    let mut system = lab.system(ALICE, "")?;
    install(&mut system)?;
    assert_eq!(
        system.runner.calls,
        [
            "/usr/bin/addgroup -S urma-node",
            "/usr/bin/adduser -S -D -H -h /var/lib/urma-node -s /usr/sbin/nologin -G urma-node urma-node",
            "/usr/bin/rc-update add urma-node default",
            "/usr/bin/rc-service urma-node start",
            "/usr/bin/adduser alice urma-node",
        ]
    );
    assert_eq!(lab.mode("etc/init.d/urma-node")?, 0o755);
    assert_eq!(lab.mode("var/log/urma-node")?, 0o750);
    let log = fs::metadata(lab.path("var/log/urma-node"))?;
    assert_eq!(log.gid(), system.runner.gid);
    fs::create_dir_all(lab.path("etc/runlevels/default"))?;
    symlink(
        "/etc/init.d/urma-node",
        lab.path("etc/runlevels/default/urma-node"),
    )?;
    system.runner.calls.clear();
    system.execute(Action::Uninstall { purge: true }, &mut Vec::new())?;
    assert_eq!(
        system.runner.calls,
        [
            "/usr/bin/rc-service urma-node stop",
            "/usr/bin/rc-update del urma-node default",
            "/usr/bin/deluser urma-node",
            "/usr/bin/delgroup urma-node",
        ]
    );
    assert!(!lab.path("etc/init.d/urma-node").exists());
    assert!(!lab.path("etc/urma-node").exists());
    assert!(!lab.path("var/lib/urma-node").exists());
    assert!(!lab.path("var/log/urma-node").exists());
    Ok(())
}

#[test]
fn runit_links_into_the_first_active_service_dir() -> Result<(), Error> {
    let lab = Lab::new(
        &[
            "run/runit",
            "etc/runit/runsvdir/default",
            "run/runit/service",
        ],
        &RUNIT,
    )?;
    let mut system = lab.system(NOBODY, "")?;
    install(&mut system)?;
    let link = lab.path("etc/runit/runsvdir/default/urma-node");
    assert_eq!(fs::read_link(&link)?, PathBuf::from("/etc/sv/urma-node"));
    assert!(!lab.path("run/runit/service/urma-node").is_symlink());
    assert_eq!(
        system.runner.calls.len(),
        1,
        "runsvdir starts a linked service by itself"
    );
    assert_eq!(lab.mode("var/log/urma-node")?, 0o750);
    let sv_status = "/usr/bin/sv status /etc/sv/urma-node";
    system
        .runner
        .answer(sv_status, &["run: /etc/sv/urma-node: (pid 7) 3s\n"]);
    point_control_at(&lab, closed_port()?)?;
    assert_eq!(
        status(&mut system)?,
        "{\"init\":\"runit\",\"registered\":true,\"enabled\":true,\"running\":true,\"listening\":false}\n"
    );
    system.runner.calls.clear();
    install(&mut system)?;
    assert_eq!(
        system.runner.calls,
        ["/usr/bin/sv restart /etc/sv/urma-node"]
    );
    fs::create_dir_all(lab.path("etc/sv/urma-node/supervise"))?;
    fs::write(lab.path("etc/sv/urma-node/supervise/ok"), "")?;
    fs::create_dir_all(lab.path("etc/sv/urma-node/log/supervise"))?;
    fs::write(lab.path("etc/sv/urma-node/supervise/status"), "")?;
    system.runner.answer(
        sv_status,
        &[
            "run: /etc/sv/urma-node: (pid 7) 3s, want down\n",
            "down: /etc/sv/urma-node: 1s; run: log: (pid 6) 9s\n",
            "fail: /etc/sv/urma-node: runsv not running\n",
        ],
    );
    system.runner.calls.clear();
    system.execute(Action::Uninstall { purge: false }, &mut Vec::new())?;
    assert_eq!(
        system.runner.calls,
        [
            "/usr/bin/sv -w 30 down /etc/sv/urma-node",
            "/usr/bin/sv exit /etc/sv/urma-node/log",
            "/usr/bin/sv exit /etc/sv/urma-node",
            sv_status,
            sv_status,
            sv_status,
        ],
        "the service dir goes only after its runsv has exited"
    );
    assert!(!link.is_symlink());
    assert!(!lab.path("etc/sv/urma-node").exists());
    assert!(lab.path("var/lib/urma-node").is_dir());
    assert_eq!(system.registration()?, Registration::Absent);
    Ok(())
}
