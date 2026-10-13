//! A peer's inventory only covers what that peer scanned. A node whose archive
//! starts later answers below its start with an empty page; that page must
//! never become coverage, so `peer_synced_through` only crosses heights some
//! peer actually scanned. Real TCP/Noise/Yamux endpoints on owned loopback.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/inventory_node.rs"]
mod inventory_node;
use bitcoin::Block;
use common::{SignedPair, profile, witness_block};
use inventory_node::{Client, Running, height, server_from, tip};
use serde_json::Value;
use std::{thread, time::Duration};
use urma_node::engine::Engine;

const LATE_START: usize = 2;

fn pairs() -> Vec<SignedPair> {
    ["proof-post", "proof-reply", "proof-avatar-ega16-reference"]
        .into_iter()
        .map(SignedPair::load)
        .chain([profile()])
        .collect()
}

// One record per block, so every height below the late start holds a record.
fn blocks(pairs: &[SignedPair]) -> Vec<Block> {
    let mut built: Vec<Block> = Vec::new();
    for (nonce, pair) in (1..).zip(pairs) {
        let block = witness_block(&pair.transactions(), built.last(), nonce);
        built.push(block);
    }
    built
}

fn synced_through(status: &Value) -> Option<u64> {
    height(&status["archive"]["peer_synced_through"])
}

fn stored(status: &Value) -> u64 {
    status["archive"]["stored_transactions"].as_u64().unwrap()
}

fn peer<'a>(status: &'a Value, label: &str) -> &'a Value {
    let peers = status["overlay"]["peers"].as_array().unwrap();
    peers.iter().find(|peer| peer["label"] == label).unwrap()
}

fn held(engine: &Engine, pair: &SignedPair) -> bool {
    engine.record(pair.reveal.compute_txid()).unwrap().is_some()
}

#[test]
fn a_later_starting_peer_never_covers_heights_below_its_own_start() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let pairs = pairs();
    let blocks = blocks(&pairs);
    let late = server_from(
        &root.join("late"),
        &blocks,
        LATE_START,
        blocks.len() - LATE_START,
    );
    let early = server_from(&root.join("early"), &blocks, 0, blocks.len());
    let client = Client::new(root, &blocks);
    let late_label = late.address.peer_id().to_string();

    // Only the late peer: its records above its start arrive, and its empty
    // answer for [start, late start) records no coverage at all.
    let running = Running::start(client.offline(), vec![late.address.clone()]);
    let mut seen = running.until(|status| stored(status) == 4);
    thread::sleep(Duration::from_secs(3));
    seen.push(running.status());
    running.stop();
    for status in &seen {
        assert_eq!(synced_through(status), None, "{status}");
    }
    let last = seen.last().unwrap();
    assert!(peer(last, &late_label)["last_error"].is_null(), "{last}");
    let engine = client.offline();
    let below = &pairs[..LATE_START];
    assert!(below.iter().all(|pair| !held(&engine, pair)));
    assert!(pairs[LATE_START..].iter().all(|pair| held(&engine, pair)));
    assert_eq!(engine.peer_synced_through().unwrap(), None);
    drop(engine);

    // The early peer covers the start; together the coverage is contiguous.
    let peers = vec![late.address.clone(), early.address.clone()];
    let running = Running::start(client.offline(), peers);
    let last = running.until(|status| synced_through(status) == Some(tip(&blocks)));
    let last = last.last().unwrap();
    assert_eq!(height(&peer(last, &late_label)["synced_through"]), None);
    running.stop();
    let engine = client.offline();
    assert!(pairs.iter().all(|pair| held(&engine, pair)));
}

#[test]
fn a_peer_starting_above_our_tip_never_counts_as_progress() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let pairs = pairs();
    let blocks = blocks(&pairs);
    let late = server_from(
        &root.join("late"),
        &blocks,
        LATE_START,
        blocks.len() - LATE_START,
    );
    // Our selected tip ends just below the peer's archive start.
    let client = Client::new(root, &blocks[..LATE_START]);
    let label = late.address.peer_id().to_string();
    let running = Running::start(client.offline(), vec![late.address.clone()]);
    let listed = |status: &Value| {
        let peers = status["overlay"]["peers"].as_array().unwrap();
        peers.iter().any(|peer| peer["label"] == label)
    };
    running.until(listed);
    // Several scheduler turns: the peer serves nothing, so it stays idle
    // (evictable) instead of looking as fresh as a fully synced peer.
    thread::sleep(Duration::from_secs(5));
    let status = running.status();
    running.stop();
    let late_peer = peer(&status, &label);
    assert!(late_peer["last_progress_at"].is_null(), "{status}");
    assert!(late_peer["last_error"].is_null(), "{status}");
    assert_eq!(synced_through(&status), None);
    assert_eq!(stored(&status), 0);
}
