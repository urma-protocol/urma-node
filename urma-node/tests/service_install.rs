#![cfg(unix)]

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use urma_node::install::{
    Action, Error, Host, Init, InitChoice, Outcome, Registration, Runner, System,
};

struct Fake {
    root: PathBuf,
    uid: u32,
    gid: u32,
    calls: Vec<String>,
    answers: HashMap<String, Outcome>,
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

impl Runner for Fake {
    fn run(&mut self, program: &str, args: &[&str]) -> Result<Outcome, Error> {
        let call = [&[program], args].concat().join(" ");
        self.calls.push(call.clone());
        match (program, args.first().copied()) {
            ("useradd", ..) | ("adduser", Some("-S")) => self.create_account()?,
            ("addgroup", Some("-S")) => {
                self.append("etc/group", &format!("urma-node:x:{}:", self.gid))?
            }
            ("usermod", ..) => self.add_member(args[3])?,
            ("adduser", Some(user)) => self.add_member(user)?,
            _ => {}
        }
        match self.answers.get(&call) {
            Some(answer) => Ok(answer.clone()),
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
        Ok(Self { dir })
    }

    fn system(&self, sudo_user: &str, pkexec_uid: &str) -> Result<System<Fake>, Error> {
        let root = self.dir.path().to_path_buf();
        let meta = fs::metadata(&root)?;
        Ok(System {
            root: root.clone(),
            host: Host {
                sudo_user: sudo_user.to_owned(),
                pkexec_uid: pkexec_uid.to_owned(),
                path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/usr/sbin")],
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

const SHADOW: [&str; 3] = ["useradd", "usermod", "userdel"];

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
            lab.system("", "")?.detect()?,
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
        let mut system = lab.system("", "")?;
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
    let system = lab.system("", "")?;
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
fn systemd_install_creates_the_account_and_starts_once() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SHADOW)?;
    let mut system = lab.system("alice", "")?;
    let out = install(&mut system)?;
    assert_eq!(
        system.runner.calls,
        [
            "useradd --system --home-dir /var/lib/urma-node --no-create-home --shell /usr/sbin/nologin --user-group urma-node",
            "systemctl daemon-reload",
            "systemctl enable --now urma-node.service",
            "usermod -a -G urma-node alice",
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
    assert_eq!(
        system.registration()?,
        Registration::Registered(Init::Systemd)
    );
    Ok(())
}

#[test]
fn a_second_install_is_an_update() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SHADOW)?;
    let mut system = lab.system("alice", "")?;
    install(&mut system)?;
    let unit = fs::read_to_string(lab.path("etc/systemd/system/urma-node.service"))?;
    system.runner.calls.clear();
    let out = install(&mut system)?;
    assert_eq!(
        system.runner.calls,
        [
            "systemctl daemon-reload",
            "systemctl restart urma-node.service"
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
            "systemctl daemon-reload",
            "systemctl restart urma-node.service"
        ]
    );
    Ok(())
}

#[test]
fn the_windows_adapter_is_refused_before_any_change() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SHADOW)?;
    let mut system = lab.system("alice", "")?;
    let init = InitChoice::Windows;
    let result = system.execute(Action::Install { init }, &mut Vec::new());
    assert!(matches!(result, Err(Error::Failed(..))));
    assert!(system.runner.calls.is_empty());
    assert!(!lab.path("etc/urma-node").exists());
    Ok(())
}

#[test]
fn update_without_a_registration_fails() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SHADOW)?;
    let mut system = lab.system("", "")?;
    let result = system.execute(Action::Update, &mut Vec::new());
    assert!(matches!(result, Err(Error::NotInstalled)));
    assert!(system.runner.calls.is_empty());
    Ok(())
}

#[test]
fn status_reports_json() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SHADOW)?;
    let mut system = lab.system("", "")?;
    assert_eq!(
        status(&mut system)?,
        "{\"init\":\"systemd\",\"registered\":false,\"enabled\":false,\"running\":false}\n"
    );
    install(&mut system)?;
    let answers = [
        ("systemctl is-enabled urma-node.service", "enabled\n"),
        ("systemctl is-active urma-node.service", "active\n"),
    ];
    for (call, stdout) in answers {
        system.runner.answers.insert(call.to_owned(), ok(stdout));
    }
    assert_eq!(
        status(&mut system)?,
        "{\"init\":\"systemd\",\"registered\":true,\"enabled\":true,\"running\":true}\n"
    );
    Ok(())
}

#[test]
fn without_an_invoker_install_prints_the_usermod_command() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SHADOW)?;
    let mut system = lab.system("root", "")?;
    let out = install(&mut system)?;
    assert_eq!(
        out,
        "To read the node token as your desktop user, run: sudo usermod -a -G urma-node <user>, then log in again.\n"
    );
    assert!(
        !system
            .runner
            .calls
            .iter()
            .any(|call| call.starts_with("usermod"))
    );
    Ok(())
}

#[test]
fn pkexec_uid_names_the_invoker() -> Result<(), Error> {
    let lab = Lab::new(&["run/systemd/system"], &SHADOW)?;
    let mut system = lab.system("", "1000")?;
    install(&mut system)?;
    assert!(
        system
            .runner
            .calls
            .iter()
            .any(|call| call == "usermod -a -G urma-node alice")
    );
    Ok(())
}

#[test]
fn openrc_install_and_purge_with_busybox() -> Result<(), Error> {
    let lab = Lab::new(
        &["run/openrc"],
        &["adduser", "addgroup", "deluser", "delgroup"],
    )?;
    let mut system = lab.system("alice", "")?;
    install(&mut system)?;
    assert_eq!(
        system.runner.calls,
        [
            "addgroup -S urma-node",
            "adduser -S -D -H -h /var/lib/urma-node -s /usr/sbin/nologin -G urma-node urma-node",
            "rc-update add urma-node default",
            "rc-service urma-node start",
            "adduser alice urma-node",
        ]
    );
    assert_eq!(lab.mode("etc/init.d/urma-node")?, 0o755);
    assert_eq!(lab.mode("var/lib/urma-node/log")?, 0o750);
    fs::create_dir_all(lab.path("etc/runlevels/default"))?;
    std::os::unix::fs::symlink(
        "/etc/init.d/urma-node",
        lab.path("etc/runlevels/default/urma-node"),
    )?;
    system.runner.calls.clear();
    system.execute(Action::Uninstall { purge: true }, &mut Vec::new())?;
    assert_eq!(
        system.runner.calls,
        [
            "rc-service urma-node stop",
            "rc-update del urma-node default",
            "deluser urma-node",
            "delgroup urma-node",
        ]
    );
    assert!(!lab.path("etc/init.d/urma-node").exists());
    assert!(!lab.path("etc/urma-node").exists());
    assert!(!lab.path("var/lib/urma-node").exists());
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
        &["sv", "useradd", "usermod"],
    )?;
    let mut system = lab.system("", "")?;
    install(&mut system)?;
    let link = lab.path("etc/runit/runsvdir/default/urma-node");
    assert_eq!(fs::read_link(&link)?, PathBuf::from("/etc/sv/urma-node"));
    assert!(!lab.path("run/runit/service/urma-node").is_symlink());
    assert_eq!(
        system.runner.calls.len(),
        1,
        "runsvdir starts a linked service by itself"
    );
    system.runner.answers.insert(
        "sv status /etc/sv/urma-node".to_owned(),
        ok("run: /etc/sv/urma-node: (pid 7) 3s\n"),
    );
    assert_eq!(
        status(&mut system)?,
        "{\"init\":\"runit\",\"registered\":true,\"enabled\":true,\"running\":true}\n"
    );
    system.runner.calls.clear();
    install(&mut system)?;
    assert_eq!(system.runner.calls, ["sv restart /etc/sv/urma-node"]);
    fs::create_dir_all(lab.path("etc/sv/urma-node/supervise"))?;
    fs::write(lab.path("etc/sv/urma-node/supervise/ok"), "")?;
    system.runner.calls.clear();
    system.execute(Action::Uninstall { purge: false }, &mut Vec::new())?;
    assert_eq!(system.runner.calls, ["sv down /etc/sv/urma-node"]);
    assert!(!link.is_symlink());
    assert!(!lab.path("etc/sv/urma-node").exists());
    assert!(lab.path("var/lib/urma-node").is_dir());
    assert_eq!(system.registration()?, Registration::Absent);
    Ok(())
}
