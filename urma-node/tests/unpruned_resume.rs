#[allow(dead_code)]
mod common;

use bitcoin::{Block, Transaction, Txid, consensus::serialize};
use common::{FIRST_HEIGHT, MemorySource, SignedPair, config, point, witness_block};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::Duration,
};
use urma_chain::{inclusion::TransactionProof, observation::Chain};
use urma_core::multipart::MultipartRecord;
use urma_node::{
    Error,
    engine::Engine,
    model::{API_VERSION, RecordPackage, Retention},
    retrieval::RecordSource,
};

// Unchanged signed public fixtures in witness-authenticated mock blocks. The
// root sits in the scanned application start; its leaf and data parts sit in
// the next selected block, which is not scanned, so only a peer supplies them.
const SLICE: Duration = Duration::from_millis(300);
const PEER_DELAY: Duration = Duration::from_millis(200);
const MAX_ROUNDS: usize = 20;

struct Object {
    pairs: Vec<SignedPair>,
    root_block: Block,
    child_block: Block,
}

impl Object {
    fn load() -> Self {
        let pairs: Vec<_> = ["root", "leaf", "part-0", "part-1"]
            .map(|suffix| SignedPair::load(&format!("multipart/two-{suffix}")))
            .into();
        let root_block = witness_block(&pairs[0].transactions(), None, 1);
        let children: Vec<Transaction> = pairs[1..]
            .iter()
            .flat_map(SignedPair::transactions)
            .collect();
        let child_block = witness_block(&children, Some(&root_block), 2);
        Self {
            pairs,
            root_block,
            child_block,
        }
    }

    fn root(&self) -> Txid {
        self.pairs[0].reveal.compute_txid()
    }

    fn payload_bytes(&self) -> u64 {
        self.pairs[2..]
            .iter()
            .map(|pair| {
                let MultipartRecord::Data(part) = pair.verified.decode().unwrap() else {
                    panic!("data fixture")
                };
                part.payload.len() as u64
            })
            .sum()
    }

    fn child_packages(&self) -> BTreeMap<Txid, RecordPackage> {
        self.pairs[1..]
            .iter()
            .map(|pair| {
                let txid = pair.reveal.compute_txid();
                let position = self
                    .child_block
                    .txdata
                    .iter()
                    .position(|tx| tx.compute_txid() == txid)
                    .unwrap();
                let package = RecordPackage {
                    version: API_VERSION,
                    chain_genesis: Chain::BitcoinRegtest.genesis().unwrap().0,
                    block: point(FIRST_HEIGHT + 1, &self.child_block),
                    reveal: TransactionProof::from_block(
                        &self.child_block,
                        position as u32,
                        pair.raw_reveal.clone(),
                        serialize(&self.child_block.txdata[0]),
                    )
                    .unwrap(),
                    commit: pair.raw_commit.clone(),
                };
                (txid, package)
            })
            .collect()
    }

    fn engine(&self, directory: &std::path::Path, retention: Retention) -> Engine {
        let blocks = [self.root_block.clone(), self.child_block.clone()];
        let source = Arc::new(MemorySource::new(&blocks));
        let engine =
            Engine::open(directory, config(&self.root_block, retention), Some(source)).unwrap();
        engine.synchronize_headers().unwrap();
        engine.scan_next().unwrap();
        engine
    }
}

// Serves the children after `delay`; PEER_DELAY takes most of a slice, so no
// single round can finish the object.
struct Peer {
    packages: BTreeMap<Txid, RecordPackage>,
    delay: Duration,
    calls: Mutex<Vec<Txid>>,
}

impl Peer {
    fn new(object: &Object, delay: Duration) -> Self {
        Self {
            packages: object.child_packages(),
            delay,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn empty() -> Self {
        Self {
            packages: BTreeMap::new(),
            delay: Duration::ZERO,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Txid> {
        self.calls.lock().unwrap().clone()
    }
}

impl RecordSource for Peer {
    fn fetch(&self, txid: Txid) -> Result<Option<RecordPackage>, Error> {
        self.calls.lock().unwrap().push(txid);
        thread::sleep(self.delay);
        Ok(self.packages.get(&txid).cloned())
    }
}

#[test]
fn unpruned_object_completes_across_slices_without_refetching_stored_records() {
    let object = Object::load();
    let directory = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let engine = object.engine(directory.path(), Retention::Unpruned);
    for child in &object.pairs[1..] {
        assert!(
            engine
                .record(child.reveal.compute_txid())
                .unwrap()
                .is_none()
        );
    }
    let peer = Peer::new(&object, PEER_DELAY);
    let cancelled = AtomicBool::new(false);

    let mut rounds = 0;
    let status = loop {
        assert!(rounds < MAX_ROUNDS, "object never completed");
        rounds += 1;
        // An unfinished slice is progress: Ok, never an error for status.
        match engine
            .retain_next_object(&[&peer], SLICE, scratch.path(), &cancelled)
            .unwrap()
        {
            Some(status) => break status,
            None => assert!(engine.object_status(object.root()).unwrap().is_none()),
        }
    };

    assert!(
        rounds > 1,
        "a slice shorter than the fetches must take several rounds"
    );
    assert_eq!(status.root, object.root());
    assert_eq!(status.payload_bytes, object.payload_bytes());
    assert_eq!(status.members_total, 4);
    assert!(status.selected_complete());
    for pair in &object.pairs {
        let stored = engine.record(pair.reveal.compute_txid()).unwrap().unwrap();
        assert_eq!(stored.package.reveal.transaction, pair.raw_reveal);
    }
    // Every record a peer delivered was persisted, even one that arrived after
    // its slice ended, so no record is ever fetched twice.
    let mut calls = peer.calls();
    calls.sort();
    let fetched = calls.len();
    calls.dedup();
    assert_eq!(fetched, calls.len(), "a stored record was fetched again");
    assert_eq!(calls.len(), 3);
}

#[test]
fn stored_members_reverify_outside_the_slice_so_the_round_fetches_the_next_missing_one() {
    let object = Object::load();
    let directory = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let engine = object.engine(directory.path(), Retention::Unpruned);
    let packages = object.child_packages();
    let missing = object.pairs[3].reveal.compute_txid();
    for pair in &object.pairs[1..3] {
        engine
            .admit_record(packages[&pair.reveal.compute_txid()].clone())
            .unwrap();
    }
    let peer = Peer::new(&object, Duration::ZERO);
    // Re-verifying the stored root, leaf and first part alone takes far longer
    // than this slice, so a slice that also counted local work would never
    // reach the missing part.
    let slice = Duration::from_nanos(1);

    let status = engine
        .retain_next_object(&[&peer], slice, scratch.path(), &AtomicBool::new(false))
        .unwrap()
        .expect("one round completes the object from its stored members");

    assert_eq!(peer.calls(), [missing]);
    assert_eq!(status.root, object.root());
    assert_eq!(status.members_total, 4);
    assert!(status.selected_complete());
}

#[test]
fn records_the_first_source_lacks_come_from_the_next_source() {
    let object = Object::load();
    let directory = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let engine = object.engine(directory.path(), Retention::Unpruned);
    let first = Peer::empty();
    let second = Peer::new(&object, Duration::ZERO);

    let status = engine
        .retain_next_object(
            &[&first, &second],
            SLICE,
            scratch.path(),
            &AtomicBool::new(false),
        )
        .unwrap()
        .expect("the second source completes the object");

    assert_eq!(first.calls().len(), 3);
    assert_eq!(second.calls().len(), 3);
    assert_eq!(status.members_total, 4);
    assert!(status.selected_complete());
}

#[test]
fn pruned_scan_keeps_the_root_omits_children_and_acquires_nothing() {
    let object = Object::load();
    let whole: Vec<Transaction> = object
        .pairs
        .iter()
        .flat_map(SignedPair::transactions)
        .collect();
    let block = witness_block(&whole, None, 3);
    let directory = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let source = Arc::new(MemorySource::new(std::slice::from_ref(&block)));
    let engine = Engine::open(
        directory.path(),
        config(&block, Retention::Pruned),
        Some(source),
    )
    .unwrap();
    engine.synchronize_headers().unwrap();
    engine.scan_next().unwrap();
    let peer = Peer::new(&object, PEER_DELAY);

    assert!(
        engine
            .retain_next_object(&[&peer], SLICE, scratch.path(), &AtomicBool::new(false))
            .unwrap()
            .is_none()
    );
    assert!(peer.calls().is_empty());
    assert!(engine.record(object.root()).unwrap().is_some());
    for child in &object.pairs[1..] {
        assert!(
            engine
                .record(child.reveal.compute_txid())
                .unwrap()
                .is_none()
        );
    }
    assert!(engine.object_status(object.root()).unwrap().is_none());
}
