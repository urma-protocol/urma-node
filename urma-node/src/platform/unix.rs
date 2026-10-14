use super::Exposure;
use std::{
    fs::{DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt as _},
    path::Path,
};
use urma_io::Access;

struct Modes;

impl Modes {
    const SETTINGS_DIRECTORY: u32 = 0o755;
    const STATE_DIRECTORY: u32 = 0o750;
    const PUBLIC_FILE: u32 = 0o644;
}

pub(super) fn create_settings_directory(path: &Path) -> Result<(), io::Error> {
    DirBuilder::new()
        .recursive(true)
        .mode(Modes::SETTINGS_DIRECTORY)
        .create(path)
}

pub(super) fn create_state_directory(path: &Path) -> Result<(), io::Error> {
    DirBuilder::new()
        .recursive(true)
        .mode(Modes::STATE_DIRECTORY)
        .create(path)
}

pub(super) fn create_data_directory(path: &Path) -> Result<(), io::Error> {
    urma_io::create_private_directory(path)
}

pub(super) fn create_file(path: &Path, exposure: Exposure) -> Result<File, io::Error> {
    match exposure {
        Exposure::Private => urma_io::create_file(path, Access::Private),
        Exposure::Readers => urma_io::create_file(path, Access::Shared),
        Exposure::Public => OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(Modes::PUBLIC_FILE)
            .open(path),
    }
}
