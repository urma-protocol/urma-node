use super::ServeOptions;
#[cfg(windows)]
use super::scm;
#[cfg(not(windows))]
use super::serve;
use crate::{
    Error, install,
    platform::{Paths, ServiceSettings},
    service::ShutdownHandle,
};
use std::{io::Write, path::PathBuf};

#[derive(Clone, Debug, clap::Subcommand)]
pub enum ServiceCommand {
    /// Serve with service.json: what every init adapter and the Windows
    /// service manager start.
    Run {
        #[arg(long, hide = true, default_value_os_t = Paths::system().settings)]
        settings: PathBuf,
    },
    #[command(flatten)]
    Manage(install::Action),
}

/// `urma-node service run`. On Windows a start by the service manager is
/// detected and served under it; otherwise this is a foreground run whose
/// stop signal `signals` installs.
pub fn run(
    settings: &std::path::Path,
    signals: impl FnOnce(ShutdownHandle) -> Result<(), Error>,
    log: &mut impl Write,
) -> Result<(), Error> {
    let options = ServeOptions::from_settings(ServiceSettings::read(settings)?);
    #[cfg(windows)]
    {
        scm::run(options, signals, log)
    }
    #[cfg(not(windows))]
    {
        serve(options, signals, log)
    }
}
