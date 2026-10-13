//! Multi-version concurrency control over a `BTreeMap<Vec<u8>, Vec<Version>>`.
//!
//! Snapshot isolation with first-committer-wins conflict detection:
//! every transaction captures a read timestamp, reads only versions at or
//! below it (plus its own uncommitted writes), and at commit time fails
//! with `DbError::Conflict` if any key it wrote was committed by another
//! transaction after its snapshot.
//!
//! Durability: every commit is appended to the WAL (with fsync) *before*
//! becoming visible in memory. Recovery replays the WAL.

use crate::wal::{Wal, WalRecord};
use quorum_types::{DbError, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

pub type Timestamp = u64;
pub type TxnId = u64;

#[derive(Debug, Clone, PartialEq)]
struct VersionedValue {
    /// `None` = tombstone.
    value: Option<Vec<u8>>,
    ts: Timestamp,
}

#[derive(Debug, Default)]
struct WriteSet {
    puts: BTreeMap<Vec<u8>, Vec<u8>>,
    deletes: BTreeSet<Vec<u8>>,
}

impl WriteSet {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.deletes.remove(&key);
        self.puts.insert(key, value);
    }

    fn delete(&mut self, key: Vec<u8>) {
        self.puts.remove(&key);
        self.deletes.insert(key);
    }

    fn is_empty(&self) -> bool {
        self.puts.is_empty() && self.deletes.is_empty()
    }

    fn clear(&mut self) {
        self.puts.clear();
        self.deletes.clear();
    }
}

/// An open transaction: snapshot + buffered writes.
#[derive(Debug)]
pub struct Txn {
    pub(crate) id: TxnId,
    pub(crate) read_ts: Timestamp,
    pub(crate) writes: WriteSet,
}

impl Txn {
    pub fn read_ts(&self) -> Timestamp {
        self.read_ts
    }

    pub fn id(&self) -> TxnId {
        self.id
    }
}

struct Inner {
    data: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    /// Last commit timestamp per key (for conflict detection).
    last_commit: BTreeMap<Vec<u8>, Timestamp>,
    /// Next commit timestamp to hand out. Visible snapshot = `next_ts - 1`.
    next_ts: Timestamp,
    next_txn: TxnId,
    wal: Wal,
}

pub struct MvccStore {
    inner: Mutex<Inner>,
}

impl MvccStore {
    /// On-disk store: creates `dir`, replays its WAL.
    pub fn open(path: &Path) -> Result<MvccStore> {
        std::fs::create_dir_all(path)?;
        let wal_path = path.join("wal.log");
        let mut data: BTreeMap<Vec<u8>, Vec<VersionedValue>> = BTreeMap::new();
        let mut last_commit: BTreeMap<Vec<u8>, Timestamp> = BTreeMap::new();
        let mut next_ts: Timestamp = 1;
        for rec in Wal::replay(&wal_path)? {
            match rec {
                WalRecord::Commit { ts, entries } => {
                    for (key, value) in entries {
                        data.entry(key.clone()).or_default().insert(
                            0,
                            VersionedValue {
                                value,
                                ts,
                            },
                        );
                        last_commit.insert(key, ts);
                    }
                    next_ts = next_ts.max(ts + 1);
                }
            }
        }
        let wal = Wal::open(&wal_path)?;
        Ok(MvccStore {
            inner: Mutex::new(Inner {
                data,
                last_commit,
                next_ts,
                next_txn: 1,
                wal,
            }),
        })
    }

    /// Volatile store (no WAL). Used for tests and `:memory:` databases.
    pub fn in_memory() -> Result<MvccStore> {
        Ok(MvccStore {
            inner: Mutex::new(Inner {
                data: BTreeMap::new(),
                last_commit: BTreeMap::new(),
                next_ts: 1,
                next_txn: 1,
                wal: Wal::memory(),
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Begin a snapshot-isolation transaction.
    pub fn begin(&self) -> Txn {
        let mut inner = self.lock();
        let read_ts = inner.next_ts - 1;
        let id = inner.next_txn;
        inner.next_txn += 1;
        Txn {
            id,
            read_ts,
            writes: WriteSet::default(),
        }
    }

    /// Point read: own writes first, then committed versions <= snapshot.
    pub fn get(&self, txn: &Txn, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(v) = txn.writes.puts.get(key) {
            return Ok(Some(v.clone()));
        }
        if txn.writes.deletes.contains(key) {
            return Ok(None);
        }
        let inner = self.lock();
        Ok(inner
            .data
            .get(key)
            .and_then(|versions| versions.iter().find(|v| v.ts <= txn.read_ts))
            .and_then(|v| v.value.clone()))
    }

    /// Prefix scan honoring the snapshot and own writes.
    /// Returns results ordered by key.
    pub fn scan(&self, txn: &Txn, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        {
            let inner = self.lock();
            let start = prefix.to_vec();
            for (key, versions) in inner.data.range(start..) {
                if !key.starts_with(prefix) {
                    break;
                }
                if let Some(v) = versions.iter().find(|v| v.ts <= txn.read_ts) {
                    if let Some(val) = &v.value {
                        out.insert(key.clone(), val.clone());
                    }
                }
            }
        }
        for (k, v) in &txn.writes.puts {
            if k.starts_with(prefix) {
                out.insert(k.clone(), v.clone());
            }
        }
        for k in &txn.writes.deletes {
            if k.starts_with(prefix) {
                out.remove(k);
            }
        }
        Ok(out.into_iter().collect())
    }

    pub fn put(&self, txn: &mut Txn, key: &[u8], value: Vec<u8>) {
        txn.writes.put(key.to_vec(), value);
    }

    pub fn delete(&self, txn: &mut Txn, key: &[u8]) {
        txn.writes.delete(key.to_vec());
    }

    /// Commit: conflict check, WAL append, then apply to the in-memory
    /// versioned map. Returns the commit timestamp.
    pub fn commit(&self, txn: Txn) -> Result<Timestamp> {
        if txn.writes.is_empty() {
            return Ok(txn.read_ts); // read-only txn
        }
        let mut inner = self.lock();

        // First-committer-wins: someone committed one of our keys after
        // our snapshot was taken.
        for key in txn.writes.puts.keys().chain(txn.writes.deletes.iter()) {
            if let Some(&ts) = inner.last_commit.get(key) {
                if ts > txn.read_ts {
                    return Err(DbError::Conflict(format!(
                        "txn {} aborted: key modified after snapshot",
                        txn.id
                    )));
                }
            }
        }

        let ts = inner.next_ts;
        let mut entries: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for (k, v) in &txn.writes.puts {
            entries.push((k.clone(), Some(v.clone())));
        }
        for k in &txn.writes.deletes {
            entries.push((k.clone(), None));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // Durability before visibility.
        inner
            .wal
            .append(&WalRecord::Commit { ts, entries: entries.clone() })?;

        for (key, value) in entries {
            inner
                .data
                .entry(key.clone())
                .or_default()
                .insert(0, VersionedValue { value, ts });
            inner.last_commit.insert(key, ts);
        }
        inner.next_ts = ts + 1;
        Ok(ts)
    }

    /// Abort a transaction: drop its write set.
    pub fn abort(&self, mut txn: Txn) {
        txn.writes.clear();
    }

    /// Latest committed timestamp (mostly for tests/metrics).
    pub fn now(&self) -> Timestamp {
        self.lock().next_ts - 1
    }

    /// Garbage-collect versions that no active snapshot can observe.
    /// `oldest_read_ts` must be <= every open transaction's read timestamp.
    /// Keeps at least one visible version per key.
    pub fn gc(&self, oldest_read_ts: Timestamp) -> Result<usize> {
        let mut inner = self.lock();
        let mut removed = 0usize;
        for versions in inner.data.values_mut() {
            // find the newest version <= oldest_read_ts (must survive)
            if let Some(idx) = versions.iter().position(|v| v.ts <= oldest_read_ts) {
                if idx > 0 {
                    removed += idx;
                    versions.drain(..idx);
                }
            } else {
                // all versions are newer than the oldest snapshot; keep them
            }
        }
        Ok(removed)
    }
}
