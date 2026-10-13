use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum InitChoice {
    Auto,
    Systemd,
    Openrc,
    Runit,
    Windows,
}

#[derive(Clone, Debug, clap::Subcommand)]
pub enum Action {
    #[command(about = "Register, configure and start the system service")]
    Install {
        #[arg(long, value_enum, default_value = "auto")]
        init: InitChoice,
    },
    #[command(about = "Rewrite the service registration and restart it")]
    Update,
    #[command(about = "Stop and remove the service registration")]
    Uninstall {
        #[arg(long, help = "Also remove data, configuration and the service account")]
        purge: bool,
    },
    #[command(about = "Print init kind, registration and run state as JSON")]
    Status,
}

#[derive(Clone, Debug, Default)]
pub struct Host {
    pub sudo_user: String,
    pub pkexec_uid: String,
    pub path: Vec<PathBuf>,
    pub binary: PathBuf,
}
