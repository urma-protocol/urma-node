#[cfg(windows)]
use super::scm;
#[cfg(not(windows))]
use super::{ServeOptions, serve};
#[cfg(not(windows))]
use crate::platform::ServiceSettings;
use crate::{Error, install, platform::Paths, service::ShutdownHandle};
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
    #[cfg(windows)]
    {
        scm::run(settings, signals, log)
    }
    #[cfg(not(windows))]
    {
        let options = ServeOptions::from_settings(ServiceSettings::read(settings)?);
        serve(options, signals, log)
    }
}
