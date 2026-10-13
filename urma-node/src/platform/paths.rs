//! Where the installed system service keeps its files, per operating system
//! (URMA_NODE_INSTALL.md §2).
use crate::config;
use std::path::{Path, PathBuf};

/// The installed service's files. `settings_dir` holds service.json and
/// config.json; `state_dir` holds the token, the peer key and the data dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub settings_dir: PathBuf,
    pub state_dir: PathBuf,
    pub settings: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub token: PathBuf,
    pub peer_key: PathBuf,
}

impl Paths {
    /// The layout of the running operating system.
    pub fn system() -> Self {
        #[cfg(windows)]
        {
            Self::windows_under(&config::program_data())
        }
        #[cfg(not(windows))]
        {
            Self::linux_under(Path::new("/"))
        }
    }

    /// The Linux layout below `root`: `/` on a real system, a temporary
    /// directory in tests.
    pub fn linux_under(root: &Path) -> Self {
        let settings_dir = root.join("etc/urma-node");
        let state_dir = root.join("var/lib/urma-node");
        Self::from_dirs(settings_dir, state_dir)
    }

    /// The Windows layout below `program_data`: `%ProgramData%` on a real
    /// system, a temporary directory in tests.
    pub fn windows_under(program_data: &Path) -> Self {
        let base = program_data.join(config::WINDOWS_PRODUCT_DIR);
        Self::from_dirs(base.clone(), base)
    }

    fn from_dirs(settings_dir: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            settings: settings_dir.join("service.json"),
            config: settings_dir.join("config.json"),
            data: state_dir.join("data"),
            token: state_dir.join("service.token"),
            peer_key: state_dir.join("peer.key"),
            settings_dir,
            state_dir,
        }
    }
}
