//! Who may read a file the installer creates (URMA_NODE_INSTALL.md §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// The service account and administrators only: the peer key.
    Private,
    /// Also readable by local readers: the token (group on Linux, Users on Windows).
    Readers,
    /// Readable by everyone, writable by administrators: service.json, config.json.
    Public,
}
