//! First-install state (URMA_NODE_INSTALL.md §3): every item is created only
//! when absent, nothing existing is rewritten, and nothing existing is adopted
//! unless an administrator owns it and only administrators can write it.
use super::{Exposure, Paths, ServiceSettings, os};
use crate::{Error, config, service};
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

type Writer<'a> = &'a dyn Fn(&mut File) -> Result<(), Error>;
type Create = fn(&Path) -> Result<(), io::Error>;

/// Whether an existing item must pass `urma_io::check_admin` before it is
/// adopted. The Linux state directory and everything below it belong to the
/// service account after install, so only Windows guards the token there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Adopt {
    AdminOnly,
    Owned,
}

impl Adopt {
    const STATE: Self = match cfg!(windows) {
        true => Self::AdminOnly,
        false => Self::Owned,
    };
}

/// Creates what is missing below `paths` and returns what it created. The
/// caller (install) owns the account and, on Linux, the chown to it.
pub fn ensure_defaults(paths: &Paths) -> Result<Vec<PathBuf>, Error> {
    let mut created = Vec::new();
    let directories: [(&Path, Create, Adopt); 3] = [
        (
            &paths.settings_dir,
            os::create_settings_directory,
            Adopt::AdminOnly,
        ),
        (&paths.state_dir, os::create_state_directory, Adopt::STATE),
        (&paths.data, os::create_data_directory, Adopt::Owned),
    ];
    for (path, create, adopt) in directories {
        if absent_directory(path, adopt)? {
            create(path)?;
            created.push(path.to_path_buf());
        }
    }
    let config = serde_json::to_vec_pretty(&config::service_archive_config()?)?;
    let settings = serde_json::to_vec_pretty(&ServiceSettings::defaults(paths))?;
    let files: [(&Path, Exposure, Adopt, Writer); 4] = [
        (&paths.config, Exposure::Public, Adopt::AdminOnly, &|file| {
            Ok(file.write_all(&config)?)
        }),
        (
            &paths.settings,
            Exposure::Public,
            Adopt::AdminOnly,
            &|file| Ok(file.write_all(&settings)?),
        ),
        (
            &paths.peer_key,
            Exposure::Private,
            Adopt::Owned,
            &write_peer_key,
        ),
        (
            &paths.token,
            Exposure::Readers,
            Adopt::STATE,
            &service::write_token,
        ),
    ];
    for (path, exposure, adopt, write) in files {
        if fill(path, exposure, adopt, write)? {
            created.push(path.to_path_buf());
        }
    }
    Ok(created)
}

/// A fresh Ed25519 transport identity, never a wallet or decryption key.
pub fn write_peer_key(file: &mut File) -> Result<(), Error> {
    let mut bytes = peer_key_bytes()?;
    let written = file.write_all(&bytes);
    bytes.fill(0);
    Ok(written?)
}

/// A private per-user transport identity at `path`; an existing file stays.
pub fn create_peer_key(path: &Path) -> Result<(), Error> {
    let mut bytes = peer_key_bytes()?;
    let written = urma_io::write_new(path, &bytes);
    bytes.fill(0);
    written.map_err(|error| Error::Invalid(error.to_string()))
}

fn peer_key_bytes() -> Result<Vec<u8>, Error> {
    libp2p::identity::Keypair::generate_ed25519()
        .to_protobuf_encoding()
        .map_err(|error| Error::Invalid(error.to_string()))
}

fn adoptable(path: &Path, adopt: Adopt) -> Result<(), Error> {
    match adopt {
        Adopt::Owned => Ok(()),
        Adopt::AdminOnly => match urma_io::check_admin(path) {
            Ok(()) => Ok(()),
            Err(error) => Err(Error::Invalid(format!(
                "refusing to adopt {}: it must be owned by and writable only by administrators \
                 ({error}); remove it or let `urma-node service install` reset it",
                path.display()
            ))),
        },
    }
}

fn absent_directory(path: &Path, adopt: Adopt) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            adoptable(path, adopt)?;
            Ok(false)
        }
        Ok(metadata) => Err(Error::Invalid(format!(
            "{} exists but is a {:?}, not a directory",
            path.display(),
            metadata.file_type()
        ))),
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(cause) => Err(cause.into()),
    }
}

fn fill(path: &Path, exposure: Exposure, adopt: Adopt, write: Writer) -> Result<bool, Error> {
    let mut file = match os::create_file(path, exposure) {
        Ok(file) => file,
        Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => {
            if !std::fs::symlink_metadata(path)?.is_file() {
                return Err(Error::Invalid(format!(
                    "{} exists but is not a regular file: {cause}",
                    path.display()
                )));
            }
            adoptable(path, adopt)?;
            return Ok(false);
        }
        Err(cause) => return Err(cause.into()),
    };
    let outcome = write(&mut file).and_then(|()| Ok(file.sync_all()?));
    drop(file);
    match outcome {
        Ok(()) => Ok(true),
        Err(cause) => {
            std::fs::remove_file(path)?;
            Err(cause)
        }
    }
}
