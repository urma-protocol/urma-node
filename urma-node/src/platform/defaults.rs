//! First-install state (URMA_NODE_INSTALL.md §3): every item is created only
//! when absent and nothing existing is rewritten.
use super::{Exposure, Paths, ServiceSettings, os};
use crate::{Error, config, service};
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

type Writer<'a> = &'a dyn Fn(&mut File) -> Result<(), Error>;
type Create = fn(&Path) -> Result<(), io::Error>;

/// Creates what is missing below `paths` and returns what it created. The
/// caller (install) owns the account and, on Linux, the chown to it.
pub fn ensure_defaults(paths: &Paths) -> Result<Vec<PathBuf>, Error> {
    let mut created = Vec::new();
    let directories: [(&Path, Create); 3] = [
        (&paths.settings_dir, os::create_settings_directory),
        (&paths.state_dir, os::create_state_directory),
        (&paths.data, os::create_data_directory),
    ];
    for (path, create) in directories {
        if absent_directory(path)? {
            create(path)?;
            created.push(path.to_path_buf());
        }
    }
    let config = serde_json::to_vec_pretty(&config::service_archive_config()?)?;
    let settings = serde_json::to_vec_pretty(&ServiceSettings::defaults(paths))?;
    let files: [(&Path, Exposure, Writer); 4] = [
        (&paths.config, Exposure::Public, &|file| {
            Ok(file.write_all(&config)?)
        }),
        (&paths.settings, Exposure::Public, &|file| {
            Ok(file.write_all(&settings)?)
        }),
        (&paths.peer_key, Exposure::Private, &write_peer_key),
        (&paths.token, Exposure::Readers, &service::write_token),
    ];
    for (path, exposure, write) in files {
        if fill(path, exposure, write)? {
            created.push(path.to_path_buf());
        }
    }
    Ok(created)
}

/// A fresh Ed25519 transport identity, never a wallet or decryption key.
pub fn write_peer_key(file: &mut File) -> Result<(), Error> {
    let key = libp2p::identity::Keypair::generate_ed25519();
    let mut bytes = key
        .to_protobuf_encoding()
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let written = file.write_all(&bytes);
    bytes.fill(0);
    Ok(written?)
}

fn absent_directory(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(metadata) => Err(Error::Invalid(format!(
            "{} exists but is a {:?}, not a directory",
            path.display(),
            metadata.file_type()
        ))),
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(cause) => Err(cause.into()),
    }
}

fn fill(path: &Path, exposure: Exposure, write: Writer) -> Result<bool, Error> {
    let mut file = match os::create_file(path, exposure) {
        Ok(file) => file,
        Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => {
            return match std::fs::symlink_metadata(path)?.is_file() {
                true => Ok(false),
                false => Err(Error::Invalid(format!(
                    "{} exists but is not a regular file: {cause}",
                    path.display()
                ))),
            };
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
