use serde_json::{Value, json};
use urma_chain::observation::Chain;
use urma_node::{
    daemon::{read_header_bootstrap, read_peer_key},
    platform::create_peer_key,
};
use urma_runtime::p2p::HeaderBootstrap;

fn input() -> Value {
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest);
    json!({"version":1,"chain":"bitcoin-regtest","height":0,
        "header":hex::encode(bitcoin::consensus::serialize(&genesis.header)),"ancestors":[]})
}

fn read(value: Value, application_height: u64) -> Result<HeaderBootstrap, String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bootstrap.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    read_header_bootstrap(&path, Chain::BitcoinRegtest, application_height)
        .map_err(|e| e.to_string())
}

#[test]
fn bootstrap_file_binds_version_chain_height_and_genesis() {
    let bootstrap = read(input(), 0).unwrap();
    assert_eq!(bootstrap.height(), 0);
    assert_eq!(
        bootstrap.chain().genesis().unwrap(),
        Chain::BitcoinRegtest.genesis().unwrap()
    );
    assert!(bootstrap.ancestors().is_empty());
    for (field, value, expected) in [
        ("version", json!(2), "version or chain mismatch"),
        (
            "chain",
            json!("litecoin-testnet"),
            "version or chain mismatch",
        ),
        ("height", json!(1), "application start precedes"),
        ("unexpected", json!(true), "unknown field"),
    ] {
        let mut invalid = input();
        invalid[field] = value;
        assert!(read(invalid, 0).unwrap_err().contains(expected));
    }
    let mut invalid = input();
    let mut header = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest).header;
    header.nonce ^= 1;
    invalid["header"] = json!(hex::encode(bitcoin::consensus::serialize(&header)));
    assert!(read(invalid, 0).unwrap_err().contains("genesis"));
}

#[test]
fn bootstrap_file_requires_exact_header_encoding_and_context_count() {
    for encoded in ["00".repeat(79), "00".repeat(81), "zz".repeat(80)] {
        let mut invalid = input();
        invalid["header"] = json!(encoded);
        assert!(read(invalid, 0).is_err());
    }
    let mut invalid = input();
    invalid["height"] = json!(1);
    assert!(
        read(invalid, 1)
            .unwrap_err()
            .contains("exactly 1 ancestors")
    );
    let mut invalid = input();
    invalid["ancestors"] = json!([invalid["header"].clone()]);
    assert!(
        read(invalid, 0)
            .unwrap_err()
            .contains("exactly 0 ancestors")
    );
}

#[test]
fn bootstrap_input_size_is_bounded_before_json_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bootstrap.json");
    std::fs::write(&path, vec![b' '; 1_048_577]).unwrap();
    assert!(
        read_header_bootstrap(&path, Chain::BitcoinRegtest, 0)
            .unwrap_err()
            .to_string()
            .contains("exceeds 1 MiB")
    );
}

#[test]
fn peer_identity_is_explicit_private_stable_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peer.key");
    assert!(read_peer_key(&path).is_err());
    assert!(!path.exists());
    create_peer_key(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    let id = read_peer_key(&path).unwrap().public().to_peer_id();
    assert_eq!(read_peer_key(&path).unwrap().public().to_peer_id(), id);
    assert!(create_peer_key(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_peer_key(&path).is_err());
    }
}

#[test]
fn invalid_transport_keys_are_refused_without_replacement() {
    let dir = tempfile::tempdir().unwrap();
    for bytes in [vec![], vec![1, 2, 3], vec![0; 4097]] {
        let path = dir.path().join("invalid.key");
        urma_io::write_replace(&path, &bytes).unwrap();
        assert!(read_peer_key(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    assert!(read_peer_key(dir.path()).is_err());
}
