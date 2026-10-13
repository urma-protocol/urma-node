use crate::{
    config::SERVICE_NAME,
    install::{Error, Runner, System, system::text},
    platform::{Paths, ensure_defaults},
};
use std::{
    fs::{self, Permissions},
    io::Write,
    os::unix::fs::{PermissionsExt, lchown},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
}

pub struct Account;

impl Account {
    pub const NAME: &'static str = SERVICE_NAME;
    const NOLOGIN: [&'static str; 3] = ["/usr/sbin/nologin", "/sbin/nologin", "/usr/bin/nologin"];
    const NO_SHELL: &'static str = "/bin/false";
}

fn number(field: &str) -> Result<u32, Error> {
    field
        .parse()
        .map_err(|error| Error::Account(format!("bad id {field:?}: {error}")))
}

impl<R: Runner> System<R> {
    fn table(&self, file: &str) -> Result<Vec<Vec<String>>, Error> {
        let content = fs::read_to_string(self.at(Path::new(file)))?;
        Ok(content
            .lines()
            .map(|line| line.split(':').map(str::to_owned).collect::<Vec<_>>())
            .filter(|fields| fields.len() >= 4)
            .collect())
    }

    fn entries(&self, file: &str) -> Result<Vec<Vec<String>>, Error> {
        Ok(self
            .table(file)?
            .into_iter()
            .filter(|fields| fields[0] == Account::NAME)
            .collect())
    }

    fn has(&self, file: &str) -> Result<bool, Error> {
        Ok(!self.entries(file)?.is_empty())
    }

    fn ids(&self) -> Result<Ids, Error> {
        let entries = self.entries("/etc/passwd")?;
        let Some(fields) = entries.first() else {
            return Err(Error::Account(format!(
                "{} is missing from /etc/passwd",
                Account::NAME
            )));
        };
        Ok(Ids {
            uid: number(&fields[2])?,
            gid: number(&fields[3])?,
        })
    }

    fn members(&self) -> Result<Vec<String>, Error> {
        let entries = self.entries("/etc/group")?;
        let Some(fields) = entries.first() else {
            return Err(Error::Account(format!(
                "group {} is missing",
                Account::NAME
            )));
        };
        Ok(fields[3]
            .split(',')
            .filter(|member| !member.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn user_by_uid(&self, uid: u32) -> Result<Option<String>, Error> {
        for fields in self.table("/etc/passwd")? {
            if number(&fields[2])? == uid {
                return Ok(Some(fields[0].clone()));
            }
        }
        Ok(None)
    }

    fn nologin(&self) -> &'static str {
        for shell in Account::NOLOGIN {
            if self.at(Path::new(shell)).is_file() {
                return shell;
            }
        }
        Account::NO_SHELL
    }

    pub fn ensure_account(&mut self) -> Result<Ids, Error> {
        if self.has("/etc/passwd")? {
            return self.ids();
        }
        let grouped = self.has("/etc/group")?;
        let home = Paths::linux_under(Path::new("/")).state_dir;
        let home = text(&home)?.to_owned();
        let shell = self.nologin();
        if self.on_path("useradd") {
            let group: &[&str] = match grouped {
                true => &["--gid", Account::NAME],
                false => &["--user-group"],
            };
            let mut args = vec!["--system", "--home-dir", &home, "--no-create-home"];
            args.extend(["--shell", shell]);
            args.extend(group);
            args.push(Account::NAME);
            self.runner.require("useradd", &args)?;
        } else if self.on_path("adduser") {
            if !grouped {
                self.runner.require("addgroup", &["-S", Account::NAME])?;
            }
            let mut args = vec!["-S", "-D", "-H", "-h", &home, "-s", shell, "-G"];
            args.extend([Account::NAME, Account::NAME]);
            self.runner.require("adduser", &args)?;
        } else {
            return Err(Error::Account(
                "neither useradd nor adduser is on PATH".into(),
            ));
        }
        self.ids()
    }

    pub fn remove_account(&mut self) -> Result<(), Error> {
        if self.has("/etc/passwd")? {
            let program = match self.on_path("userdel") {
                true => "userdel",
                false => "deluser",
            };
            self.runner.require(program, &[Account::NAME])?;
        }
        if self.has("/etc/group")? {
            let program = match self.on_path("groupdel") {
                true => "groupdel",
                false => "delgroup",
            };
            self.runner.require(program, &[Account::NAME])?;
        }
        Ok(())
    }

    fn invoker(&self) -> Result<Option<String>, Error> {
        let sudo = self.host.sudo_user.trim();
        if !sudo.is_empty() && sudo != "root" {
            return Ok(Some(sudo.to_owned()));
        }
        let uid = self.host.pkexec_uid.trim();
        if uid.is_empty() {
            return Ok(None);
        }
        let uid = number(uid)?;
        match self.user_by_uid(uid)? {
            Some(name) if name != "root" => Ok(Some(name)),
            Some(..) => Ok(None),
            None => Err(Error::Account(format!(
                "PKEXEC_UID {uid} is not in /etc/passwd"
            ))),
        }
    }

    pub fn join_group(&mut self, out: &mut impl Write) -> Result<(), Error> {
        let Some(user) = self.invoker()? else {
            writeln!(
                out,
                "To read the node token as your desktop user, run: sudo usermod -a -G {} <user>, then log in again.",
                Account::NAME
            )?;
            return Ok(());
        };
        if self.members()?.contains(&user) {
            return Ok(());
        }
        match self.on_path("usermod") {
            true => self
                .runner
                .require("usermod", &["-a", "-G", Account::NAME, &user])?,
            false => self.runner.require("adduser", &[&user, Account::NAME])?,
        };
        writeln!(
            out,
            "Added {user} to group {}; log in again to read the node token.",
            Account::NAME
        )?;
        Ok(())
    }

    pub fn own(&self, paths: &Paths, ids: Ids) -> Result<(), Error> {
        ensure_defaults(paths)?;
        for path in [&paths.state_dir, &paths.data, &paths.token, &paths.peer_key] {
            lchown(path, Some(ids.uid), Some(ids.gid))?;
        }
        Ok(())
    }

    pub fn log_directory(&self, log: &Path, ids: Ids) -> Result<(), Error> {
        fs::create_dir_all(log)?;
        lchown(log, Some(ids.uid), Some(ids.gid))?;
        fs::set_permissions(log, Permissions::from_mode(0o750))?;
        Ok(())
    }
}
