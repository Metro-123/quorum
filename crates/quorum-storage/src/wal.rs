//! Write-ahead log.
//!
//! Frame layout: `u32 payload_len || payload`. Every commit record is
//! followed by `fsync`. A torn tail (crash mid-write) is tolerated on
//! replay: decoding stops at the first truncated or corrupt frame.

use quorum_types::codec::{put_bool, put_bytes, put_u32, put_u64, put_u8, Reader};
use quorum_types::{DbError, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum WalRecord {
    /// A committed transaction: timestamp plus the key/value mutation set.
    /// `Some(value)` is a put, `None` is a delete.
    Commit {
        ts: u64,
        entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    },
}

impl WalRecord {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            WalRecord::Commit { ts, entries } => {
                put_u8(&mut out, 1);
                put_u64(&mut out, *ts);
                put_u32(&mut out, entries.len() as u32);
                for (key, value) in entries {
                    put_bytes(&mut out, key);
                    match value {
                        Some(v) => {
                            put_bool(&mut out, true);
                            put_bytes(&mut out, v);
                        }
                        None => put_bool(&mut out, false),
                    }
                }
            }
        }
        out
    }

    fn decode(payload: &[u8]) -> Result<WalRecord> {
        let mut r = Reader::new(payload);
        match r.u8()? {
            1 => {
                let ts = r.u64()?;
                let n = r.u32()? as usize;
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = r.bytes()?.to_vec();
                    let value = if r.bool()? {
                        Some(r.bytes()?.to_vec())
                    } else {
                        None
                    };
                    entries.push((key, value));
                }
                Ok(WalRecord::Commit { ts, entries })
            }
            t => Err(DbError::Storage(format!("unknown WAL record tag {}", t))),
        }
    }
}

pub struct Wal {
    /// `None` for the in-memory engine.
    file: Option<File>,
}

impl Wal {
    pub fn memory() -> Wal {
        Wal { file: None }
    }

    pub fn open(path: &Path) -> Result<Wal> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Wal { file: Some(file) })
    }

    pub fn append(&mut self, rec: &WalRecord) -> Result<()> {
        let file = match &mut self.file {
            Some(f) => f,
            None => return Ok(()),
        };
        let payload = rec.encode();
        let mut frame = Vec::with_capacity(4 + payload.len());
        put_u32(&mut frame, payload.len() as u32);
        frame.extend_from_slice(&payload);
        file.write_all(&frame)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    /// Read all intact records. Missing file => empty log. Torn tail => stop.
    pub fn replay(path: &Path) -> Result<Vec<WalRecord>> {
        let mut buf = Vec::new();
        match File::open(path) {
            Ok(mut f) => {
                f.read_to_end(&mut buf)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        }

        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 4 <= buf.len() {
            let len =
                u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
            pos += 4;
            if pos + len > buf.len() {
                break; // torn frame
            }
            match WalRecord::decode(&buf[pos..pos + len]) {
                Ok(rec) => out.push(rec),
                Err(_) => break, // corrupt tail
            }
            pos += len;
        }
        Ok(out)
    }
}
