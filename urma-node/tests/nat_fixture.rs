//! Writes the three archives of the CI NAT simulation (.github/scripts/
//! nat-test.sh). Not a check: run once with URMA_FIXTURE_DIR set. No network.
//! P, A and B select one mock Litecoin testnet chain; only B scans it, so
//! every record A holds came over the overlay and none from P or a scan.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod common;
use bitcoin::Block;
use common::{Body, FIRST_HEIGHT, MemorySource, SignedPair, point, profile, witness_block};
use std::{fs, path::Path, process::Command, sync::Arc};
use urma_chain::observation::Chain;
use urma_node::{
    engine::Engine,
    model::{ArchiveConfig, Retention},
};

fn binary(dir: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_urma-node"))
        .env_clear()
        .arg("--data")
        .arg(dir.join("data"))
        .arg("--config")
        .arg(dir.join("config.json"))
        .args(args)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{args:?}: {stderr}");
}

// One signed vector pair per block.
fn blocks() -> Vec<Block> {
    let pairs = ["proof-post", "proof-reply", "proof-avatar-ega16-reference"]
        .into_iter()
        .map(SignedPair::load)
        .chain([profile()]);
    let mut built: Vec<Block> = Vec::new();
    for (nonce, pair) in (60_100..).zip(pairs) {
        let block = witness_block(&pair.transactions(), built.last(), nonce);
        built.push(block);
    }
    built
}

// Headers selected to the tip; B also scans every block.
fn node(dir: &Path, blocks: &[Block], scan: bool) {
    fs::create_dir_all(dir).unwrap();
    let config = ArchiveConfig {
        chain: Chain::LitecoinTestnet,
        application_start: point(FIRST_HEIGHT, &blocks[0]),
        retention: Retention::Unpruned,
    };
    fs::write(
        dir.join("config.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    let bodies = blocks.iter().map(Body::from).collect();
    let source = Arc::new(MemorySource::with_bodies(
        Chain::LitecoinTestnet,
        FIRST_HEIGHT,
        bodies,
    ));
    let engine = Engine::open(&dir.join("data"), config, Some(source)).unwrap();
    engine.synchronize_headers().unwrap();
    if scan {
        for _ in blocks {
            engine.scan_next().unwrap();
        }
    }
    drop(engine);
    let token = dir.join("service.token");
    let key = dir.join("peer.key");
    binary(dir, &["create-token", token.to_str().unwrap()]);
    binary(dir, &["create-peer-key", key.to_str().unwrap()]);
}

#[test]
#[ignore = "writes a fixture under URMA_FIXTURE_DIR; not a standalone check"]
fn write_nat_fixture() {
    let root = std::env::var_os("URMA_FIXTURE_DIR").expect("URMA_FIXTURE_DIR");
    let root = Path::new(&root);
    assert!(!root.exists(), "fixture directory must be new");
    let blocks = blocks();
    node(&root.join("p"), &blocks, false);
    node(&root.join("a"), &blocks, false);
    node(&root.join("b"), &blocks, true);
}
