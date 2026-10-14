use crate::{
    install::{
        Error,
        account::{Account, Ids},
    },
    platform::Paths,
};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, fchown},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogWriter {
    Root,
    Service,
}

pub struct Ownership;

impl Ownership {
    const LOG_MODE: u32 = 0o750;
    const PROCESS: &'static str = "/proc/self";
}

pub fn installer() -> Result<u32, Error> {
    Ok(fs::metadata(Ownership::PROCESS)?.uid())
}

fn reopen(path: &Path, kind: Kind) -> Result<File, Error> {
    let flags = match kind {
        Kind::File => libc::O_NOFOLLOW | libc::O_NONBLOCK,
        Kind::Directory => libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_DIRECTORY,
    };
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)?)
}

fn hand(path: &Path, kind: Kind, owner: Option<u32>, gid: u32) -> Result<(), Error> {
    let installer = installer()?;
    let handle = reopen(path, kind)?;
    let meta = handle.metadata()?;
    let intact = match kind {
        Kind::File => meta.is_file() && meta.nlink() == 1,
        Kind::Directory => meta.is_dir(),
    };
    if !intact || meta.uid() != installer {
        return Err(Error::Failed(format!(
            "{} changed after it was created; refusing to hand it to {}",
            path.display(),
            Account::NAME
        )));
    }
    fchown(&handle, owner, Some(gid))?;
    Ok(())
}

pub fn hand_over(created: &[PathBuf], paths: &Paths, ids: Ids) -> Result<(), Error> {
    let order = [
        (&paths.token, Kind::File),
        (&paths.peer_key, Kind::File),
        (&paths.data, Kind::Directory),
        (&paths.state_dir, Kind::Directory),
    ];
    for (path, kind) in order {
        if created.contains(path) {
            hand(path, kind, Some(ids.uid), ids.gid)?;
        }
    }
    Ok(())
}

pub fn log_directory(path: &Path, writer: LogWriter, ids: Ids) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(meta) => match meta.is_dir() {
            true => Ok(()),
            false => Err(Error::Failed(format!(
                "{} exists but is not a directory",
                path.display()
            ))),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let Some(parent) = path.parent() else {
                return Err(Error::Failed(format!("{} has no parent", path.display())));
            };
            fs::create_dir_all(parent)?;
            DirBuilder::new().mode(Ownership::LOG_MODE).create(path)?;
            let owner = match writer {
                LogWriter::Root => None,
                LogWriter::Service => Some(ids.uid),
            };
            hand(path, Kind::Directory, owner, ids.gid)
        }
        Err(error) => Err(error.into()),
    }
}
