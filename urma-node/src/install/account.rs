use crate::{
    config::SERVICE_NAME,
    install::{Error, Runner, System, system::text},
    platform::Paths,
};
use std::{fs, io::Write, path::Path};

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
    const ROOT: &'static str = "root";
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
        if self.has_program("useradd") {
            let group: &[&str] = match grouped {
                true => &["--gid", Account::NAME],
                false => &["--user-group"],
            };
            let mut args = vec!["--system", "--home-dir", &home, "--no-create-home"];
            args.extend(["--shell", shell]);
            args.extend(group);
            args.push(Account::NAME);
            self.exec("useradd", &args)?;
        } else if self.has_program("adduser") {
            if !grouped {
                self.exec("addgroup", &["-S", Account::NAME])?;
            }
            let mut args = vec!["-S", "-D", "-H", "-h", &home, "-s", shell, "-G"];
            args.extend([Account::NAME, Account::NAME]);
            self.exec("adduser", &args)?;
        } else {
            return Err(Error::Account(
                "neither useradd nor adduser is installed".into(),
            ));
        }
        self.ids()
    }

    pub fn remove_account(&mut self) -> Result<(), Error> {
        if self.has("/etc/passwd")? {
            let program = match self.has_program("userdel") {
                true => "userdel",
                false => "deluser",
            };
            self.exec(program, &[Account::NAME])?;
        }
        if self.has("/etc/group")? {
            let program = match self.has_program("groupdel") {
                true => "groupdel",
                false => "delgroup",
            };
            self.exec(program, &[Account::NAME])?;
        }
        Ok(())
    }

    fn pkexec_invoker(&self) -> Result<Option<String>, Error> {
        let uid = number(self.host.pkexec_uid.trim())?;
        for fields in self.table("/etc/passwd")? {
            if number(&fields[2])? == uid && fields[0] != Account::ROOT {
                return Ok(Some(fields[0].clone()));
            }
        }
        Ok(None)
    }

    fn sudo_invoker(&self) -> Result<Option<String>, Error> {
        let user = self.host.sudo_user.trim();
        let claimed = self.host.sudo_uid.trim();
        if user.is_empty() || user == Account::ROOT || claimed.is_empty() {
            return Ok(None);
        }
        let claimed = number(claimed)?;
        for fields in self.table("/etc/passwd")? {
            if fields[0] == user && number(&fields[2])? == claimed {
                return Ok(Some(user.to_owned()));
            }
        }
        Ok(None)
    }

    fn invoker(&self) -> Result<Option<String>, Error> {
        match self.host.pkexec_uid.trim().is_empty() {
            false => self.pkexec_invoker(),
            true => self.sudo_invoker(),
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
        match self.has_program("usermod") {
            true => self.exec("usermod", &["-a", "-G", Account::NAME, &user])?,
            false => self.exec("adduser", &[&user, Account::NAME])?,
        };
        writeln!(
            out,
            "Added {user} to group {}; log in again to read the node token.",
            Account::NAME
        )?;
        Ok(())
    }
}
