//! Append-only write-ahead log for the content-addressed store.
//!
//! DESIGN §3 / §4: the receiver accumulates the per-session log in a
//! content-addressed store. If that store is in-memory only, a receiver
//! restart forces a full cold-start re-transfer of the ~200 MB log — which
//! would weaken the "cold resume is paid ONCE" bound. This WAL makes the
//! store **durable**: every insert/reference is appended to a sequential
//! file, and on restart the store is rebuilt by replaying the log. The
//! append is a buffered sequential write (a memcpy into the BufWriter); the
//! expensive `fsync` is deferred to an explicit [`Wal::flush`], so the hot
//! path stays cheap while crash-safety is tunable.
//!
//! Record format (all little-endian):
//!   `session_id : 16` | `variant : 1` | `len : u4` | `bytes[len]`
//! `variant`:
//!   0 = insert     — `bytes` is the block's canonical encoding; replay re-inserts
//!                     (appends the id to the session log, re-derives the root).
//!   1 = reference  — `bytes` is the 32-byte `block_id`; replay re-references.
//!   2 = seed      — RESYNC: `count:u32 | count*32 ids | 32 root`; replay seeds
//!                     the session log in *manifest* order (the authoritative order).
//!   3 = content   — BULK: `bytes` is canonical encoding; replay stores the block
//!                     *content only* (no session-log append; the log was seeded).
//!
//! For steady-state `insert`/`reference`, replay re-runs them in order, so session
//! logs and Merkle roots rebuild identically (the DAG is a deterministic function
//! of the ordered block ids). For a cold-started session, a `seed` record fixes the
//! manifest order up front and `content` records fill block content without
//! disturbing that order — so an out-of-order cold start replays to the same root.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use parking_lot::Mutex;

use crate::store::ContentStore;

const VAR_INSERT: u8 = 0;
const VAR_REFERENCE: u8 = 1;
/// Seed a session log at RESYNC: the full manifest id list + the client root,
/// in manifest (authoritative) order. Replay rebuilds the session log in this
/// order regardless of the BULK arrival order, so "cold resume paid ONCE"
/// survives a receiver restart even after an out-of-order cold start.
const VAR_SEED: u8 = 2;
/// Cold-start block content (BULK). Same on-disk shape as `VAR_INSERT`
/// (canonical bytes), but replayed as content-only: it fills the block map
/// WITHOUT appending to the session log (the log was seeded by `VAR_SEED`).
const VAR_CONTENT: u8 = 3;
const HEADER: usize = 16 + 1 + 4;
const MAX_RECORD_BYTES: usize = 512 * 1024 * 1024;
// NOTE: `HEADER` is still referenced by `replay` (which reads a fixed-size
// header buffer); the append path now writes the fields directly instead of
// building a `HEADER`-sized `Vec`, so this constant is no longer used for the
// write capacity hint but remains the on-disk header length.

/// Handle to an open WAL file. Append-only; concurrent appends are serialized
/// by an internal mutex. Reads (replay) happen once at open.
pub struct Wal {
    writer: Mutex<BufWriter<File>>,
}

impl Wal {
    /// Open (creating if absent) the WAL at `path`, replay it into `store`,
    /// and return a handle for further appends.
    pub fn open<P: AsRef<Path>>(path: P, store: &ContentStore) -> std::io::Result<Self> {
        let path = path.as_ref();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // A WAL has exactly one writer. Without an OS lock, two sidecars can
        // interleave records and acknowledge state that cannot be replayed.
        file.try_lock()?;
        let original_len = file.metadata()?.len();
        let valid_len = {
            let mut reader = BufReader::new(&file);
            replay(&mut reader, store)?
        };
        // A process may die between any of the small writes that form a record.
        // Remove that incomplete tail before appending; otherwise every future
        // record would sit behind an unreplayable gap and be lost on restart.
        if valid_len < original_len {
            file.set_len(valid_len)?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Append an insert record. `canonical` is the block's canonical encoding
    /// (the caller already has it, or can obtain it via `canonical_bytes`).
    /// On replay the block is re-inserted, which re-appends its id to the
    /// session log and re-derives the same Merkle root.
    pub fn append_insert(&self, session_id: u128, canonical: &[u8]) -> std::io::Result<()> {
        // Write the record header fields directly to the BufWriter instead of
        // allocating a temp `Vec` of `HEADER + canonical.len()`, copying the
        // header + payload into it, then `write_all`-ing it. The BufWriter
        // already batches the four small writes into one logical record; the
        // on-disk format is byte-identical and the per-durable-append
        // payload-sized allocation disappears.
        let len = record_len(canonical.len())?;
        let mut w = self.writer.lock();
        w.write_all(&session_id.to_le_bytes())?;
        w.write_all(&[VAR_INSERT])?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(canonical)
    }

    /// Append a reference record (dedup path: the block is already stored, only
    /// the session-log append must be replayed).
    pub fn append_reference(&self, session_id: u128, id: [u8; 32]) -> std::io::Result<()> {
        let mut w = self.writer.lock();
        w.write_all(&session_id.to_le_bytes())?;
        w.write_all(&[VAR_REFERENCE])?;
        w.write_all(&32u32.to_le_bytes())?;
        w.write_all(&id)
    }

    /// Append a SEED record: the session's full manifest id list + client root.
    /// Recorded once at RESYNC so replay rebuilds the session log in manifest
    /// order. Payload layout: `count:u32 | count*32 id bytes | 32 root bytes`.
    pub fn append_seed(
        &self,
        session_id: u128,
        ids: &[[u8; 32]],
        root: &[u8; 32],
    ) -> std::io::Result<()> {
        let payload_len = ids
            .len()
            .checked_mul(32)
            .and_then(|len| len.checked_add(36))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "WAL seed too large")
            })?;
        let len = record_len(payload_len)?;
        let count = u32::try_from(ids.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "WAL seed has too many ids",
            )
        })?;
        let mut w = self.writer.lock();
        w.write_all(&session_id.to_le_bytes())?;
        w.write_all(&[VAR_SEED])?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(&count.to_le_bytes())?;
        for id in ids {
            w.write_all(&id[..])?;
        }
        w.write_all(&root[..])
    }

    /// Append a CONTENT record: cold-start block content (BULK). Replayed as
    /// content-only (no session-log append). Same payload shape as `append_insert`.
    pub fn append_content(&self, session_id: u128, canonical: &[u8]) -> std::io::Result<()> {
        let len = record_len(canonical.len())?;
        let mut w = self.writer.lock();
        w.write_all(&session_id.to_le_bytes())?;
        w.write_all(&[VAR_CONTENT])?;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(canonical)
    }

    /// Flush the in-process buffer. With `sync=true`, also fsync the file
    /// descriptor so records survive a crash. Batching flushes (e.g. once per
    /// turn or per flush tick) keeps the hot path off the fsync latency.
    pub fn flush(&self, sync: bool) -> std::io::Result<()> {
        let mut w = self.writer.lock();
        w.flush()?;
        if sync {
            w.get_ref().sync_data()?;
        }
        Ok(())
    }
}

fn record_len(len: usize) -> std::io::Result<u32> {
    if len > MAX_RECORD_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("WAL record length {len} exceeds safety limit"),
        ));
    }
    u32::try_from(len)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "WAL record too large"))
}

fn replay<R: Read + Seek>(r: &mut R, store: &ContentStore) -> std::io::Result<u64> {
    let mut hdr = [0u8; HEADER];
    loop {
        let record_start = r.stream_position()?;
        if !read_exact_or_eof(r, &mut hdr)? {
            return Ok(record_start);
        }
        let mut sid_bytes = [0u8; 16];
        sid_bytes.copy_from_slice(&hdr[..16]);
        let session_id = u128::from_le_bytes(sid_bytes);
        let variant = hdr[16];
        let len = u32::from_le_bytes([hdr[17], hdr[18], hdr[19], hdr[20]]) as usize;
        if len > MAX_RECORD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL record length {len} exceeds safety limit"),
            ));
        }
        let mut payload = vec![0u8; len];
        if !read_exact_or_eof(r, &mut payload)? {
            return Ok(record_start);
        }
        match variant {
            VAR_INSERT => {
                if let Ok(block) = crate::canonical::from_canonical(&payload) {
                    store.insert(session_id, block);
                }
            }
            VAR_REFERENCE => {
                if payload.len() == 32 {
                    let mut id = [0u8; 32];
                    id.copy_from_slice(&payload);
                    let _ = store.reference(session_id, id);
                }
            }
            VAR_SEED => {
                // Payload: count:u32 | count*32 ids | 32 root. Rebuild the
                // session log in manifest order. `store.seed_session` does not
                // re-log (the WAL is still unset during replay).
                if payload.len() >= 4 {
                    let count = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
                    let need = count.checked_mul(32).and_then(|n| n.checked_add(36));
                    if need == Some(payload.len()) {
                        let mut ids = Vec::with_capacity(count);
                        for i in 0..count {
                            let mut id = [0u8; 32];
                            id.copy_from_slice(&payload[4 + i * 32..4 + i * 32 + 32]);
                            ids.push(id);
                        }
                        let mut root = [0u8; 32];
                        root.copy_from_slice(&payload[4 + count * 32..4 + count * 32 + 32]);
                        store.seed_session(session_id, ids, root);
                    }
                }
            }
            VAR_CONTENT => {
                // Cold-start block content: store the block without appending to
                // the session log (the log was seeded by a prior VAR_SEED).
                if let Ok(block) = crate::canonical::from_canonical(&payload) {
                    store.store_content(session_id, block);
                }
            }
            _ => { /* unknown variant from a future version: skip */ }
        }
    }
}

/// Read exactly `buf.len()` bytes, returning false for a clean or partial EOF.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            return Ok(false);
        }
        got += n;
    }
    Ok(true)
}
