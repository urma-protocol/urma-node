//! Fast peer synchronization through v3 inventory pages between actual
//! TCP/Noise/Yamux endpoints on owned loopback. Genuine and locally signed
//! records sit in mock-selected witness-authenticated blocks: no PoW, UTXO or
//! consensus claim. Node A runs the real Service on an offline Engine whose
//! headers were selected beforehand, so every record it holds came through the
//! overlay and none from its own scan. A test-side proxy forwards to node B
//! and drops one inventory entry to play an omitting peer. A subprocess
//! supervises blocking library calls; killing it is cleanup only.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod common;
#[path = "common/inventory_node.rs"]
mod inventory_node;
#[allow(dead_code)]
#[path = "common/inventory_peer.rs"]
mod inventory_peer;
use bitcoin::{
    Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
    absolute,
    consensus::serialize,
    hashes::Hash,
    secp256k1::{Keypair as SecpKeypair, Message as Digest, Secp256k1},
    sighash::{Prevouts, SighashCache, TapSighashType},
    taproot::{LeafVersion, TapLeafHash},
    transaction::Version,
};
use common::{FIRST_HEIGHT, SignedPair, profile, witness_block};
use inventory_node::{Client, Running, bare_network, height, orphan, server, tip};
use inventory_peer::{RawPeer, Rewrite};
use libp2p::PeerId;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
use urma_chain::observation::Chain;
use urma_core::{envelope, format::PublicRecord, multipart::VerifiedRecord, topics::Topics};
use urma_node::{
    engine::Engine,
    peer_protocol::{INVENTORY_ENTRIES, INVENTORY_SPAN, Kind, Reply, VERSION},
    peerbook::PeerAddress,
    retrieval::{FetchLimits, RecordSource, fetch_record},
};

const CASE_TIMEOUT: Duration = Duration::from_secs(150);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

fn stage(root: &Path, text: &str) {
    fs::write(root.join("stage"), text).unwrap();
}

// Follows pruning_policy.rs: a distinct valid public Wire post per index.
fn wire_post(index: usize) -> SignedPair {
    let record = PublicRecord::WirePost {
        topics: Topics {
            wire: "news".into(),
            hashtags: vec!["sync".into()],
        },
        text: format!("inventory post {index}"),
    };
    let bytes = record.encode().unwrap();
    let secp = Secp256k1::new();
    let signer = SecpKeypair::from_seckey_slice(&secp, &[4; 32]).unwrap();
    let (script, info) = envelope::build(&bytes, &signer).unwrap();
    let spend = |txid, vout, value, script_pubkey| Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint { txid, vout },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey,
        }],
    };
    let output = ScriptBuf::new_p2tr_tweaked(info.output_key());
    let commit = spend(Txid::from_byte_array([8; 32]), 0, 3_000, output);
    let payout = ScriptBuf::from_bytes([vec![0, 20], vec![4; 20]].concat());
    let mut reveal = spend(commit.compute_txid(), 0, 1_500, payout);
    let hash = SighashCache::new(&reveal)
        .taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(std::slice::from_ref(&commit.output[0])),
            TapLeafHash::from_script(&script, LeafVersion::TapScript),
            TapSighashType::Default,
        )
        .unwrap();
    let signature =
        secp.sign_schnorr_no_aux_rand(&Digest::from_digest(hash.to_byte_array()), &signer);
    reveal.input[0].witness = envelope::witness(signature.as_ref(), &script, &info).unwrap();
    let verified = VerifiedRecord::verify(reveal.compute_txid(), &reveal, &commit).unwrap();
    SignedPair {
        raw_commit: serialize(&commit),
        raw_reveal: serialize(&reveal),
        commit,
        reveal,
        verified,
    }
}

// Pairs are spread over `blocks` blocks round-robin; one chain per salt.
fn chain(pairs: &[SignedPair], blocks: usize, salt: u32) -> Vec<Block> {
    let mut built: Vec<Block> = Vec::new();
    for offset in 0..blocks {
        let transactions: Vec<Transaction> = pairs
            .iter()
            .skip(offset)
            .step_by(blocks)
            .flat_map(SignedPair::transactions)
            .collect();
        let nonce = salt + u32::try_from(offset).unwrap();
        let block = witness_block(&transactions, built.last(), nonce);
        built.push(block);
    }
    built
}

fn synced_through(status: &Value) -> Option<u64> {
    height(&status["archive"]["peer_synced_through"])
}

fn assert_record(engine: &Engine, pair: &SignedPair) {
    let record = engine.record(pair.reveal.compute_txid()).unwrap();
    let record = record.unwrap_or_else(|| panic!("missing {}", pair.reveal.compute_txid()));
    assert_eq!(record.package.commit, pair.raw_commit);
    assert_eq!(record.package.reveal.transaction, pair.raw_reveal);
}

fn vectors() -> Vec<SignedPair> {
    ["proof-post", "proof-reply", "proof-avatar-ega16-reference"]
        .into_iter()
        .map(SignedPair::load)
        .chain([profile()])
        .collect()
}

fn honest(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    assert_eq!(b.network.peer_id(), b.address.peer_id());
    let b_status = serde_json::to_value(b.engine.status().unwrap()).unwrap();
    let b_calls = b.chain.body_calls.load(Ordering::Relaxed);
    let a = Client::new(root, &blocks);
    stage(root, "A synchronizes from B through inventory pages");
    let running = Running::start(a.offline(), vec![b.address.clone()]);
    let seen = running.until(|s| synced_through(s) == Some(tip(&blocks)));
    let last = seen.last().unwrap();
    assert!(last["archive"]["indexed_through"].is_null());
    assert_eq!(last["archive"]["scanned_blocks"], 0);
    let peer = &last["overlay"]["peers"][0];
    assert_eq!(height(&peer["synced_through"]), Some(tip(&blocks)));
    assert!(peer["last_error"].is_null(), "{peer}");
    running.stop();
    let engine = a.offline();
    for pair in &pairs {
        assert_record(&engine, pair);
    }
    drop(engine);
    // Serving inventory and packages is a read for B.
    assert_eq!(
        serde_json::to_value(b.engine.status().unwrap()).unwrap(),
        b_status
    );
    assert_eq!(b.chain.body_calls.load(Ordering::Relaxed), b_calls);
    // A's own later scan finds nothing the honest peer left out.
    let scanned = a.scan_all(&blocks);
    assert!(scanned.peer_omissions().unwrap().is_empty());
    assert!(scanned.omitting_peers().unwrap().is_empty());
    json!({"records":pairs.len(),"synced":true,"omissions":0})
}

fn omission(root: &Path) -> Value {
    let pairs = vectors();
    let mut blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let hidden = pairs[1].reveal.compute_txid();
    let liar = RawPeer::start(Some((b.address.clone(), Rewrite::Hide(hidden.to_string()))));
    let a = Client::new(root, &blocks);
    stage(root, "A takes coverage from an omitting proxy");
    let running = Running::start(a.offline(), vec![liar.address()]);
    running.until(|s| synced_through(s) == Some(tip(&blocks)));
    running.stop();
    let engine = a.offline();
    assert!(engine.record(hidden).unwrap().is_none());
    drop(engine);
    let scanned = a.scan_all(&blocks);
    let liar_id = liar.address().peer_id().to_string();
    let found = scanned.peer_omissions().unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].peer.as_str(), found[0].txid),
        (liar_id.as_str(), hidden)
    );
    assert_eq!(scanned.omitting_peers().unwrap(), vec![liar_id]);
    let status = serde_json::to_value(scanned.status().unwrap()).unwrap();
    assert_eq!(status["peer_omissions"], 1);
    drop(scanned);
    // The chain grows; only the honest peer is asked for the new height.
    let extra = wire_post(9_000);
    let next = witness_block(&extra.transactions(), blocks.last(), 77);
    blocks.push(next);
    b.follow(&blocks);
    a.select(&blocks);
    let asked = liar.asked();
    let running = Running::start(a.offline(), vec![liar.address(), b.address.clone()]);
    running.until(|s| synced_through(s) == Some(tip(&blocks)));
    running.stop();
    assert_eq!(liar.asked(), asked, "scheduler still asks an omitting peer");
    assert_record(&a.offline(), &extra);
    json!({"omission":hidden.to_string(),"liar_skipped":true})
}

fn listed<'a>(status: &'a Value, id: &PeerId) -> Option<&'a Value> {
    let label = id.to_string();
    status["overlay"]["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["label"].as_str() == Some(label.as_str()))
}

/// A starts with nothing configured and only listens; B dials A. A syncs
/// inventory over B's inbound connection, resolves a parent through it with
/// an overlay commit source that names no peer, and drops B once B leaves.
fn inbound(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let b_id = b.address.peer_id();
    let a = Client::new(root, &blocks);
    let running = Running::listening(a.offline());
    assert_eq!(running.status()["overlay"]["peers"], json!([]));
    stage(root, "B dials A; A syncs from its inbound peer");
    let deadline = Instant::now() + Duration::from_secs(10);
    let cancelled = AtomicBool::new(false);
    b.network
        .dial(running.listen_address(), &cancelled, deadline)
        .unwrap();
    running.until(|s| {
        listed(s, &b_id).is_some_and(|b| height(&b["synced_through"]) == Some(tip(&blocks)))
    });
    stage(
        root,
        "A resolves a parent through B with no configured peer",
    );
    let orphan = orphan(root);
    let overlay = running.commit_source(Chain::BitcoinRegtest);
    let resolved = orphan.resolve_next_dependency(&overlay, &cancelled);
    assert_eq!(resolved.unwrap(), 1);
    assert_eq!(orphan.status().unwrap().unresolved_dependencies, 0);
    stage(root, "B leaves; A drops it from the sync set");
    b.network.shutdown().unwrap();
    running.until(|s| listed(s, &b_id).is_none());
    running.stop();
    let engine = a.offline();
    for pair in &pairs {
        assert_record(&engine, pair);
    }
    json!({"synced":true,"parent_resolved":true,"left":true})
}

/// Retrieval gets each overlay peer as its own source: a corrupt package from
/// the first is rejected by verification and the next peer supplies it.
fn retrieval(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let liar = RawPeer::start(Some((b.address.clone(), Rewrite::Corrupt)));
    let a = Client::new(root, &blocks);
    let engine = a.offline();
    let network = bare_network(engine.clone(), false);
    stage(root, "a corrupt package first, the honest peer second");
    let sources = network
        .sync_peers(vec![liar.address(), b.address.clone()])
        .record_sources();
    assert_eq!(sources.len(), 2);
    let sources: Vec<&dyn RecordSource> = sources.iter().map(|s| s as &dyn RecordSource).collect();
    let txid = pairs[0].reveal.compute_txid();
    let record = fetch_record(&engine, txid, &sources, FetchLimits::default()).unwrap();
    assert_eq!(record.map(|record| record.txid()), Some(txid));
    assert_eq!(liar.asked(), 1, "the corrupt peer was asked first");
    assert_record(&engine, &pairs[0]);
    json!({"fetched":true,"liar_asked":liar.asked()})
}

/// Seven inbound peers that never answer fill the window behind one cold
/// configured peer that answers only after 4 s. The configured peer gets a
/// full call bound (an even split would leave it 2.5 s), resolves the parent,
/// and the stalling peers are never asked.
fn stalling(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let slow = RawPeer::start(Some((
        b.address.clone(),
        Rewrite::Slow(Duration::from_secs(4)),
    )));
    let a = Client::new(root, &blocks);
    let network = bare_network(a.offline(), true);
    let to = PeerAddress::new(network.peer_id(), network.listen_address().unwrap()).unwrap();
    let stalls: Vec<RawPeer> = (0..7).map(|_| RawPeer::gossiping(Vec::new())).collect();
    for stall in &stalls {
        let reply = stall.call(&to, VERSION, Kind::Peers { limit: 0 });
        assert!(matches!(reply, Reply::Peers(_)), "{reply:?}");
    }
    let live = network.sync_peers(Vec::new());
    let deadline = Instant::now() + Duration::from_secs(10);
    while live.sources(&[]).len() < stalls.len() {
        assert!(
            Instant::now() < deadline,
            "stalling peers never became live"
        );
        thread::sleep(Duration::from_millis(50));
    }
    stage(
        root,
        "seven stalling inbound peers and one slow configured peer",
    );
    let orphan = orphan(root);
    let overlay = network.commit_source(Chain::BitcoinRegtest, vec![slow.address()]);
    let began = Instant::now();
    let resolved = orphan.resolve_next_dependency(&overlay, &AtomicBool::new(false));
    assert_eq!(resolved.unwrap(), 1);
    let took = began.elapsed();
    assert!(
        took >= Duration::from_secs(4) && took < Duration::from_secs(10),
        "{took:?}"
    );
    for stall in &stalls {
        let asked = stall.requests();
        assert!(
            asked.iter().all(|r| matches!(r.kind, Kind::Peers { .. })),
            "{asked:?}"
        );
    }
    json!({"resolved":true,"stalls_asked":0})
}

fn peer_status<'a>(status: &'a Value, id: &PeerId) -> &'a Value {
    let label = id.to_string();
    status["overlay"]["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["label"].as_str() == Some(label.as_str()))
        .unwrap_or_else(|| panic!("no status for {label}"))
}

/// Three proxies list every entry in an unselected block (a stale fork) ahead
/// of one honest peer and an honest mirror listing records already held.
/// Stale turns are never blamed, but three of them in a row release the
/// stale peer's slot; the honest peer syncs within 60 s, and the mirror's
/// held-record listing counts as progress, so it is never released.
fn stale_crowd(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let stale: Vec<RawPeer> = (0..3)
        .map(|_| RawPeer::start(Some((b.address.clone(), Rewrite::Stale))))
        .collect();
    let mirror = RawPeer::start(Some((b.address.clone(), Rewrite::Hide(String::new()))));
    let stale_ids: Vec<PeerId> = stale.iter().map(|p| p.address().peer_id()).collect();
    let (honest_id, mirror_id) = (b.address.peer_id(), mirror.address().peer_id());
    let a = Client::new(root, &blocks);
    stage(root, "a stale crowd releases its slots to the honest peer");
    let mut peers: Vec<PeerAddress> = stale.iter().map(RawPeer::address).collect();
    peers.extend([b.address.clone(), mirror.address()]);
    let began = Instant::now();
    let running = Running::start(a.offline(), peers);
    let seen = running
        .until(|s| height(&peer_status(s, &honest_id)["synced_through"]) == Some(tip(&blocks)));
    let synced_in = began.elapsed();
    assert!(synced_in < Duration::from_secs(60), "{synced_in:?}");
    let honest = peer_status(seen.last().unwrap(), &honest_id);
    assert!(honest["last_progress_at"].is_u64(), "{honest}");
    let released = running.until(|s| {
        stale_ids
            .iter()
            .all(|id| peer_status(s, id)["backoff_until"].is_u64())
    });
    for id in &stale_ids {
        let entry = peer_status(released.last().unwrap(), id);
        assert!(entry["last_progress_at"].is_null(), "{entry}");
        assert!(entry["last_error"].is_null(), "stale turns blamed: {entry}");
    }
    let kept = running.until(|s| peer_status(s, &mirror_id)["last_progress_at"].is_u64());
    let entry = peer_status(kept.last().unwrap(), &mirror_id);
    assert!(entry["backoff_until"].is_null(), "mirror released: {entry}");
    assert!(entry["last_error"].is_null(), "{entry}");
    running.stop();
    for pair in &pairs {
        assert_record(&a.offline(), pair);
    }
    json!({"honest_synced":true,"stale_released":stale_ids.len(),"seconds":synced_in.as_secs()})
}

/// Ten honest proxies against eight outbound slots: the first eight sync and
/// progressed within the last minute, so the two refused by the established
/// limit evict nobody. For ten samples after both starved peers had their
/// turn no peer is evicted or held off (every eviction holds its victim off)
/// and the slots never overflow: no eviction ping-pong. A held peer may still
/// leave through the 20 s idle timeout and a starved one take its slot; that
/// is the transport freeing a slot, not churn caused by sync.
fn starved(root: &Path) -> Value {
    let pairs: Vec<SignedPair> = vectors().into_iter().take(1).collect();
    let blocks = chain(&pairs, 1, 1);
    let b = server(root, &blocks, 1);
    let proxies: Vec<RawPeer> = (0..10)
        .map(|_| RawPeer::start(Some((b.address.clone(), Rewrite::Hide(String::new())))))
        .collect();
    let ids: Vec<PeerId> = proxies.iter().map(|p| p.address().peer_id()).collect();
    let a = Client::new(root, &blocks);
    stage(
        root,
        "two peers starved by the connection limit evict nobody",
    );
    let running = Running::start(a.offline(), proxies.iter().map(RawPeer::address).collect());
    running.until(|s| {
        ids[8..].iter().all(|id| {
            let error = peer_status(s, id)["last_error"]
                .as_str()
                .unwrap_or_default();
            error.contains("established connection limit")
        })
    });
    let held = running.connected();
    assert_eq!(held.len(), 8, "{held:?}");
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(300));
        let status = running.status();
        assert!(running.connected().len() <= 8, "{}", status["overlay"]);
        for id in &ids {
            let entry = peer_status(&status, id);
            assert!(entry["backoff_until"].is_null(), "evicted: {entry}");
        }
    }
    running.stop();
    json!({"stable_samples":10,"held":held.len()})
}

fn rejecting(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let liar = RawPeer::start(Some((b.address.clone(), Rewrite::OutOfRange)));
    let (liar_id, honest_id) = (liar.address().peer_id(), b.address.peer_id());
    let a = Client::new(root, &blocks);
    stage(
        root,
        "a rejecting peer is backed off while the honest peer serves",
    );
    let running = Running::start(a.offline(), vec![liar.address(), b.address.clone()]);
    let seen = running.until(|s| {
        height(&peer_status(s, &honest_id)["synced_through"]) == Some(tip(&blocks))
            && peer_status(s, &liar_id)["backoff_until"].is_u64()
    });
    let last = seen.last().unwrap();
    let blamed = peer_status(last, &liar_id);
    let error = blamed["last_error"].as_str().unwrap_or_default();
    assert!(error.contains("out of range"), "{blamed}");
    assert!(
        peer_status(last, &honest_id)["last_error"].is_null(),
        "{last}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while running.connected().contains(&liar_id) {
        assert!(Instant::now() < deadline, "rejecting peer still connected");
        thread::sleep(Duration::from_millis(100));
    }
    let asked = liar.asked();
    thread::sleep(Duration::from_secs(2));
    assert_eq!(liar.asked(), asked, "a backed-off peer was asked again");
    running.stop();
    for pair in &pairs {
        assert_record(&a.offline(), pair);
    }
    json!({"blamed":true,"disconnected":true,"honest_synced":true})
}

fn paging(root: &Path) -> Value {
    let pairs: Vec<SignedPair> = (0..70).map(wire_post).collect();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let a = Client::new(root, &blocks);
    stage(root, "70 records over 4 blocks span several rounds");
    let running = Running::start(a.offline(), vec![b.address.clone()]);
    let seen = running.until(|s| synced_through(s) == Some(tip(&blocks)));
    running.stop();
    let mut previous = (None, None, 0);
    let mut partial = false;
    for status in &seen {
        let peer = &status["overlay"]["peers"][0];
        let sample = (
            height(&peer["cursor"]),
            synced_through(status),
            status["archive"]["stored_transactions"].as_u64().unwrap(),
        );
        assert!(sample.0 >= previous.0 && sample.1 >= previous.1, "{seen:?}");
        assert!(sample.2 >= previous.2, "stored transactions shrank");
        partial |= sample.2 > 0 && sample.2 < 140;
        previous = sample;
    }
    assert!(partial, "fetch budget did not span rounds");
    let engine = a.offline();
    for pair in &pairs {
        assert_record(&engine, pair);
    }
    json!({"records":pairs.len(),"spanned_rounds":partial})
}

fn reorg(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 4);
    let a = Client::new(root, &blocks);
    let running = Running::start(a.offline(), vec![b.address.clone()]);
    running.until(|s| synced_through(s) == Some(tip(&blocks)));
    running.stop();
    // A new last block deselects the old one and its record.
    stage(root, "reorg of the last block after coverage");
    let fresh = wire_post(7_000);
    let mut branch = blocks[..3].to_vec();
    branch.push(witness_block(&fresh.transactions(), branch.last(), 500));
    b.follow(&branch);
    a.select(&branch);
    let engine = a.offline();
    let dropped = height(&serde_json::to_value(engine.peer_synced_through().unwrap()).unwrap());
    assert_eq!(dropped, Some(tip(&branch) - 1));
    let old = pairs[3].reveal.compute_txid();
    assert!(
        engine.record(old).unwrap().is_none(),
        "orphan record selected"
    );
    drop(engine);
    let running = Running::start(a.offline(), vec![b.address.clone()]);
    running.until(|s| synced_through(s) == Some(tip(&branch)));
    running.stop();
    assert_record(&a.offline(), &fresh);
    json!({"truncated_to":dropped,"resynced":true})
}

fn inventory(from_height: u64, to_height: u64, limit: u32) -> Kind {
    Kind::Inventory {
        from_height,
        to_height,
        after: None,
        limit,
    }
}

fn limits(root: &Path) -> Value {
    let pairs = vectors();
    let blocks = chain(&pairs, 4, 1);
    let b = server(root, &blocks, 2);
    let client = RawPeer::start(None);
    let to = b.address.clone();
    stage(root, "raw inventory requests against B");
    // Span is to_height - from_height: exactly INVENTORY_SPAN is allowed.
    let widest = inventory(FIRST_HEIGHT, FIRST_HEIGHT + INVENTORY_SPAN, 8);
    let allowed = client.call(&to, VERSION, widest);
    assert!(matches!(allowed, Reply::Inventory(_)), "{allowed:?}");
    let wide = inventory(FIRST_HEIGHT, FIRST_HEIGHT + INVENTORY_SPAN + 1, 8);
    let wide = client.call(&to, VERSION, wide);
    assert!(matches!(wide, Reply::Refused), "{wide:?}");
    let greedy = inventory(FIRST_HEIGHT, FIRST_HEIGHT + 1, INVENTORY_ENTRIES + 1);
    let greedy = client.call(&to, VERSION, greedy);
    assert!(matches!(greedy, Reply::Refused), "{greedy:?}");
    let empty_limit = client.call(&to, VERSION, inventory(FIRST_HEIGHT, FIRST_HEIGHT, 0));
    assert!(matches!(empty_limit, Reply::Refused), "{empty_limit:?}");
    let old = client.call(&to, 2, inventory(FIRST_HEIGHT, FIRST_HEIGHT + 1, 8));
    assert!(matches!(old, Reply::UnsupportedVersion), "{old:?}");
    let upper = FIRST_HEIGHT + 2;
    let Reply::Inventory(empty) = client.call(&to, VERSION, inventory(upper, upper + 1, 8)) else {
        panic!("unscanned range refused");
    };
    assert!(empty.entries.is_empty());
    assert_eq!(
        height(&json!(empty.scanned_through)),
        Some(FIRST_HEIGHT + 1)
    );
    let full = inventory(FIRST_HEIGHT, upper - 1, INVENTORY_ENTRIES);
    let Reply::Inventory(page) = client.call(&to, VERSION, full) else {
        panic!("scanned range refused");
    };
    // Record placements only, in (height, position) order.
    let listed: Vec<Txid> = page.entries.iter().map(|entry| entry.txid).collect();
    let expected: Vec<Txid> = pairs[..2].iter().map(|p| p.reveal.compute_txid()).collect();
    assert_eq!(listed, expected);
    let order: Vec<(u64, u32)> = page
        .entries
        .iter()
        .map(|e| (e.height, e.position))
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    assert!(!page.more);
    json!({"refused":3,"unscanned_empty":true,"listed":page.entries.len()})
}

#[test]
#[ignore = "owned subprocess host, invoked only by the inventory cases"]
fn peer_inventory_host() {
    let root = PathBuf::from(std::env::var_os("PEER_INVENTORY_TEMP").expect("owned TEMP"));
    let scenario = std::env::var("PEER_INVENTORY_SCENARIO").unwrap();
    let checks = match scenario.as_str() {
        "honest" => honest(&root),
        "inbound" => inbound(&root),
        "retrieval" => retrieval(&root),
        "stalling" => stalling(&root),
        "omission" => omission(&root),
        "rejecting" => rejecting(&root),
        "stale" => stale_crowd(&root),
        "starved" => starved(&root),
        "paging" => paging(&root),
        "reorg" => reorg(&root),
        "limits" => limits(&root),
        _ => panic!("unknown owned scenario"),
    };
    let report = json!({"scenario":scenario,"checks":checks});
    fs::write(
        root.join("result.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
}

struct OwnedChild {
    child: Option<Child>,
    directory: PathBuf,
}

impl OwnedChild {
    fn logs(&self) -> String {
        ["stage", "stdout.log", "stderr.log"]
            .map(|name| {
                let mut text = String::new();
                if let Ok(file) = File::open(self.directory.join(name)) {
                    let _ = file.take(64 * 1024).read_to_string(&mut text);
                }
                text
            })
            .join("\n")
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                other => {
                    eprintln!("inventory host reap exceeded bound: {other:?}");
                    return;
                }
            }
        }
    }
}

fn run(scenario: &str) -> Value {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "peer_inventory_host"])
        .args(["--nocapture", "--test-threads=1"])
        .env_clear()
        .env("PEER_INVENTORY_TEMP", path)
        .env("PEER_INVENTORY_SCENARIO", scenario)
        .env("TMPDIR", path)
        .stdin(Stdio::null())
        .stdout(File::create(path.join("stdout.log")).unwrap())
        .stderr(File::create(path.join("stderr.log")).unwrap())
        .spawn()
        .unwrap();
    let mut owned = OwnedChild {
        child: Some(child),
        directory: path.to_owned(),
    };
    let deadline = Instant::now() + CASE_TIMEOUT;
    loop {
        if let Some(exit) = owned.child.as_mut().unwrap().try_wait().unwrap() {
            owned.child.take();
            assert!(exit.success(), "{scenario}: {exit}; {}", owned.logs());
            let report: Value =
                serde_json::from_slice(&fs::read(path.join("result.json")).unwrap()).unwrap();
            assert_eq!(report["scenario"], scenario);
            return report["checks"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "{scenario} exceeded bound; kill is cleanup only; {}",
            owned.logs()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn fresh_node_admits_every_record_from_inventory_before_its_own_scan() {
    let checks = run("honest");
    assert_eq!(checks["synced"], true);
    assert_eq!(checks["omissions"], 0);
}

#[test]
fn a_node_with_an_empty_book_syncs_and_resolves_parents_through_an_inbound_peer() {
    let checks = run("inbound");
    assert_eq!(checks["synced"], true);
    assert_eq!(checks["parent_resolved"], true);
    assert_eq!(checks["left"], true);
}

#[test]
fn a_corrupt_package_from_one_peer_falls_through_to_the_next_peer() {
    let checks = run("retrieval");
    assert_eq!(checks["fetched"], true);
    assert_eq!(checks["liar_asked"], 1);
}

#[test]
fn stalling_inbound_peers_cannot_delay_a_configured_parent_peer() {
    let checks = run("stalling");
    assert_eq!(checks["resolved"], true);
}

#[test]
fn own_scan_detects_an_omitting_peer_and_the_scheduler_stops_asking_it() {
    let checks = run("omission");
    assert_eq!(checks["liar_skipped"], true);
}

#[test]
fn a_stale_crowd_releases_its_slots_and_the_honest_peer_syncs() {
    let checks = run("stale");
    assert_eq!(checks["honest_synced"], true);
    assert_eq!(checks["stale_released"], 3);
}

#[test]
fn peers_starved_by_the_connection_limit_do_not_evict_progressing_peers() {
    let checks = run("starved");
    assert_eq!(checks["stable_samples"], 10);
    assert_eq!(checks["held"], 8);
}

#[test]
fn a_rejecting_peer_is_disconnected_and_backed_off_while_an_honest_peer_proceeds() {
    let checks = run("rejecting");
    assert_eq!(checks["blamed"], true);
    assert_eq!(checks["disconnected"], true);
    assert_eq!(checks["honest_synced"], true);
}

#[test]
fn inventory_beyond_one_round_budget_progresses_monotonically() {
    let checks = run("paging");
    assert_eq!(checks["records"], 70);
    assert_eq!(checks["spanned_rounds"], true);
}

#[test]
fn reorg_truncates_peer_coverage_and_resync_completes() {
    let checks = run("reorg");
    assert_eq!(checks["resynced"], true);
}

#[test]
fn server_refuses_oversized_or_old_inventory_and_lists_only_scanned_blocks() {
    let checks = run("limits");
    assert_eq!(checks["refused"], 3);
    assert_eq!(checks["listed"], 2);
}
