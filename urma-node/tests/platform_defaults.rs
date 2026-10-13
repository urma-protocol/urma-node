use std::fs;
use urma_node::{
    config,
    model::ArchiveConfig,
    platform::{Paths, ServiceSettings, ensure_defaults, service_sid},
};

fn layout(root: &std::path::Path) -> Paths {
    #[cfg(windows)]
    {
        Paths::windows_under(root)
    }
    #[cfg(not(windows))]
    {
        Paths::linux_under(root)
    }
}

#[test]
fn service_sid_matches_the_windows_derivation() {
    assert_eq!(
        service_sid("TrustedInstaller"),
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    );
    assert_eq!(
        service_sid("trustedinstaller"),
        service_sid("TrustedInstaller")
    );
}

#[test]
fn first_install_creates_everything_once_and_never_rewrites() {
    let root = tempfile::tempdir().unwrap();
    let paths = layout(root.path());
    let created = ensure_defaults(&paths).unwrap();
    for path in [
        &paths.data,
        &paths.config,
        &paths.settings,
        &paths.peer_key,
        &paths.token,
    ] {
        assert!(created.contains(path), "{} not created", path.display());
    }
    let archive: ArchiveConfig = serde_json::from_slice(&fs::read(&paths.config).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(archive).unwrap(),
        serde_json::to_value(config::service_archive_config().unwrap()).unwrap()
    );
    assert_eq!(
        ServiceSettings::read(&paths.settings).unwrap(),
        ServiceSettings::defaults(&paths)
    );
    let token = urma_io::read_shared(&paths.token, 128).unwrap();
    assert_eq!(token.len(), 64);
    assert!(token.iter().all(u8::is_ascii_hexdigit));
    let key = urma_io::read_private(&paths.peer_key, 4096).unwrap();
    libp2p::identity::Keypair::from_protobuf_encoding(&key).unwrap();

    let before: Vec<Vec<u8>> = [
        &paths.config,
        &paths.settings,
        &paths.peer_key,
        &paths.token,
    ]
    .iter()
    .map(|path| fs::read(path).unwrap())
    .collect();
    assert!(ensure_defaults(&paths).unwrap().is_empty());
    let after: Vec<Vec<u8>> = [
        &paths.config,
        &paths.settings,
        &paths.peer_key,
        &paths.token,
    ]
    .iter()
    .map(|path| fs::read(path).unwrap())
    .collect();
    assert_eq!(before, after);
}

#[test]
fn an_operator_config_survives_and_only_missing_items_appear() {
    let root = tempfile::tempdir().unwrap();
    let paths = layout(root.path());
    fs::create_dir_all(&paths.settings_dir).unwrap();
    fs::write(&paths.config, b"operator").unwrap();
    let created = ensure_defaults(&paths).unwrap();
    assert!(!created.contains(&paths.config));
    assert_eq!(fs::read(&paths.config).unwrap(), b"operator");
    assert!(created.contains(&paths.token));
}

#[test]
#[cfg(unix)]
fn linux_modes_follow_the_install_contract() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::linux_under(root.path());
    ensure_defaults(&paths).unwrap();
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&paths.state_dir), 0o750);
    assert_eq!(mode(&paths.data), 0o700);
    assert_eq!(mode(&paths.peer_key), 0o600);
    assert_eq!(mode(&paths.token), 0o640);
    assert_eq!(mode(&paths.config) & 0o022, 0);
    assert_eq!(mode(&paths.config) & 0o004, 0o004);

    let planted = tempfile::tempdir().unwrap();
    let paths = Paths::linux_under(planted.path());
    fs::create_dir_all(&paths.state_dir).unwrap();
    symlink(planted.path().join("elsewhere"), &paths.token).unwrap();
    assert!(ensure_defaults(&paths).is_err());
    assert!(!planted.path().join("elsewhere").exists());
}
