//! On-disk cache files for the CSR snapshot and the property columns.
//!
//! A cold process pays a full adjacency scan to build the first CSR snapshot
//! and a full record scan to build a column set, which on a large graph
//! dominate the first aggregation's latency. Each cache file persists the
//! built structure next to the LMDB files, so a later process loads it
//! sequentially instead of rebuilding. LMDB stays the source of truth: every
//! file records the persisted commit generation it was built at (see
//! [`crate::storage::ids::commit_gen`]), and a load is refused on any
//! mismatch, so a stale, truncated, corrupt, or foreign file degrades to the
//! ordinary rebuild rather than a wrong answer.
//!
//! The save sites are deliberate and narrow: [`crate::graph::Graph::rebuild_csr`]
//! saves the CSR file, and the two column materialize methods save theirs. The
//! freshness gate's per-write refreshes never save, so a write-heavy session
//! never pays a file write per rebuild.
//!
//! The CSR format is little-endian: a magic tag, the 128-bit database
//! identity (see [`crate::storage::Storage::db_id`]), the generation, flags,
//! the two array lengths, the arrays themselves in a fixed order, and a
//! 64-bit checksum folded over every preceding byte. The `id_to_dense` map is
//! not stored; it is rebuilt from `dense_to_id` on load. The columns files
//! carry a msgpack payload behind the same header and checksum discipline.
//! The identity is what refuses a file left behind by a different database at
//! a coincidentally matching generation, which a restore into a directory
//! with leftover cache files would otherwise serve.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::array::{Array, MappedFile};
use crate::csr::CsrSnapshot;
use crate::error::Error;
use crate::schema::{EdgeId, NodeId, TypeId};

const MAGIC: &[u8; 8] = b"ISSNCSR1";
const FLAG_WEIGHTED: u64 = 1;
const FLAG_NEGATIVE_WEIGHT: u64 = 2;

/// A 64-bit checksum folded over the file's words. Corruption detection, not
/// cryptography, and word-wise rather than byte-wise so that verifying a
/// mapped file of a few hundred megabytes on every open runs at memory speed.
struct Sum64 {
    h: u64,
    buf: [u8; 8],
    buf_len: usize,
    total: u64,
}

impl Sum64 {
    fn new() -> Self {
        Sum64 {
            h: 0x9E37_79B9_7F4A_7C15,
            buf: [0; 8],
            buf_len: 0,
            total: 0,
        }
    }

    #[inline]
    fn mix(&mut self, word: u64) {
        let mut h = self.h ^ word;
        h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 32;
        self.h = h;
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if self.buf_len > 0 {
            let take = (8 - self.buf_len).min(bytes.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&bytes[..take]);
            self.buf_len += take;
            bytes = &bytes[take..];
            if self.buf_len < 8 {
                return;
            }
            self.mix(u64::from_le_bytes(self.buf));
            self.buf_len = 0;
        }
        let mut words = bytes.chunks_exact(8);
        for w in &mut words {
            let mut word = [0u8; 8];
            word.copy_from_slice(w);
            self.mix(u64::from_le_bytes(word));
        }
        let rest = words.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    fn finish(mut self) -> u64 {
        if self.buf_len > 0 {
            let mut tail = [0u8; 8];
            tail[..self.buf_len].copy_from_slice(&self.buf[..self.buf_len]);
            self.mix(u64::from_le_bytes(tail));
        }
        self.mix(self.total);
        self.h
    }
}

/// The checksum of `bytes` in one pass, for a mapped file.
fn checksum_of(bytes: &[u8]) -> u64 {
    let mut sum = Sum64::new();
    sum.update(bytes);
    sum.finish()
}

/// Checksumming writer wrapper.
struct SumWriter<W: Write> {
    inner: W,
    sum: Sum64,
}

impl<W: Write> SumWriter<W> {
    fn put(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.sum.update(bytes);
        self.inner.write_all(bytes)
    }

    fn put_u64(&mut self, v: u64) -> std::io::Result<()> {
        self.put(&v.to_le_bytes())
    }

    fn put_u64s(&mut self, vs: &[u64]) -> std::io::Result<()> {
        for &v in vs {
            self.put(&v.to_le_bytes())?;
        }
        Ok(())
    }

    fn put_usizes(&mut self, vs: &[usize]) -> std::io::Result<()> {
        for &v in vs {
            self.put(&(v as u64).to_le_bytes())?;
        }
        Ok(())
    }

    fn put_u32s(&mut self, vs: &[u32]) -> std::io::Result<()> {
        for &v in vs {
            self.put(&v.to_le_bytes())?;
        }
        Ok(())
    }

    fn put_f64s(&mut self, vs: &[f64]) -> std::io::Result<()> {
        for &v in vs {
            self.put(&v.to_le_bytes())?;
        }
        Ok(())
    }

    /// Zero bytes up to the next multiple of 8 after `written` bytes.
    fn pad_to_8(&mut self, written: usize) -> std::io::Result<()> {
        let pad = align8(written) - written;
        self.put(&[0u8; 8][..pad])
    }

    /// Write the checksum of everything put so far and flush.
    fn finish(mut self) -> std::io::Result<()> {
        let sum = self.sum.finish();
        self.inner.write_all(&sum.to_le_bytes())?;
        self.inner.flush()
    }
}

/// Little-endian `u64` at `offset`, or `None` past the end.
fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

/// Whether this target can map a cache file directly: the arrays are stored as
/// little-endian 64-bit words for `usize` fields, so a big-endian or 32-bit
/// host reads them wrongly and falls back to building from storage instead.
const fn can_map_files() -> bool {
    cfg!(all(target_pointer_width = "64", target_endian = "little"))
}

fn csr_path(dir: &Path) -> PathBuf {
    dir.join("csr.cache")
}

/// Bytes of the fixed CSR header: magic, database identity, generation, flags,
/// and the two array lengths.
const CSR_HEADER_LEN: usize = 8 + 16 + 8 + 8 + 8 + 8;

/// Persist `snap` for `commit_gen`, atomically: the bytes go to a temp file in
/// the same directory and the rename publishes them, so a crash mid-write
/// leaves either the previous cache file or none, never a torn one. The
/// arrays follow the header in a fixed order at 8-byte-aligned offsets (the two
/// 4-byte arrays of each direction are adjacent, so every 8-byte array starts
/// aligned), which is what lets a later process map the file and point the
/// snapshot's arrays straight into it.
pub(crate) fn save_csr(
    dir: &Path,
    snap: &CsrSnapshot,
    db_id: [u8; 16],
    commit_gen: u64,
) -> Result<(), Error> {
    let tmp = dir.join("csr.cache.tmp");
    let write = || -> std::io::Result<()> {
        let mut w = SumWriter {
            inner: BufWriter::new(File::create(&tmp)?),
            sum: Sum64::new(),
        };
        w.put(MAGIC)?;
        w.put(&db_id)?;
        w.put_u64(commit_gen)?;
        let mut flags = 0u64;
        if snap.edge_weight.is_some() {
            flags |= FLAG_WEIGHTED;
        }
        if snap.has_negative_weight {
            flags |= FLAG_NEGATIVE_WEIGHT;
        }
        w.put_u64(flags)?;
        w.put_u64(snap.dense_to_id.len() as u64)?;
        w.put_u64(snap.col_idx.len() as u64)?;
        w.put_u64s(&snap.dense_to_id)?;
        w.put_usizes(&snap.row_ptr)?;
        w.put_u32s(&snap.col_idx)?;
        w.put_u32s(&snap.edge_type)?;
        w.put_u64s(&snap.edge_id)?;
        w.put_usizes(&snap.in_row_ptr)?;
        w.put_u32s(&snap.in_col_idx)?;
        w.put_u32s(&snap.in_edge_type)?;
        w.put_u32s(&snap.in_pos)?;
        if let Some(weights) = &snap.edge_weight {
            // Five 4-byte arrays end 4 bytes short of an 8-byte boundary when
            // the edge count is odd; the weights are 8-byte values.
            w.put(&[0u8; 8][..weight_padding(snap.col_idx.len() as u64)])?;
            w.put_f64s(weights)?;
        }
        w.finish()
    };
    write().map_err(Error::Io)?;
    std::fs::rename(&tmp, csr_path(dir)).map_err(Error::Io)?;
    Ok(())
}

/// Total bytes a CSR cache file with `n` nodes and `e` edges must hold, or
/// `None` when the arithmetic overflows, which no real file can cause. The
/// lengths come from the file itself and decide where every array starts
/// before the checksum can vouch for them, so a corrupt length field has to be
/// refused against the file's actual size instead of trusted.
fn csr_file_len(n: u64, e: u64, weighted: bool) -> Option<u64> {
    // Header plus the trailing checksum.
    let mut total = CSR_HEADER_LEN as u64 + 8;
    // `dense_to_id`, the two `(n + 1)`-long row-pointer arrays, and `edge_id`
    // are 8 bytes per element; `col_idx`, `edge_type`, `in_col_idx`,
    // `in_edge_type`, and `in_pos` are 4 bytes per element. The five 4-byte
    // arrays total `20e` bytes, so `in_pos` (the last of them) can start on a
    // 4-byte boundary only, which is all a `u32` array needs.
    let u64_elems = n
        .checked_add(n.checked_add(1)?.checked_mul(2)?)?
        .checked_add(e)?;
    total = total.checked_add(u64_elems.checked_mul(8)?)?;
    total = total.checked_add(e.checked_mul(4)?.checked_mul(5)?)?;
    if weighted {
        total = total.checked_add(weight_padding(e) as u64)?;
        total = total.checked_add(e.checked_mul(8)?)?;
    }
    Some(total)
}

/// Bytes between the last 4-byte array and the weights, so the weights start
/// 8-byte aligned: the five 4-byte arrays total `20e` bytes.
fn weight_padding(e: u64) -> usize {
    if e % 2 == 1 { 4 } else { 0 }
}

/// Map the cache file if it exists, carries `db_id`, and reflects
/// `expected_gen`, carrying weights when `want_weights` asks for them. `None`
/// on a missing, stale, truncated, corrupt, foreign, or version-mismatched
/// file, on an unweighted cache file when weights are wanted, and on a target
/// that cannot map the format. Every refusal means "build from storage
/// instead", never an error, because the file is a cache and storage can
/// always answer.
///
/// The checksum is verified over the whole file before any array is served,
/// which reads every page once, sequentially; after that the pages belong to
/// the page cache and only the ones a query touches stay resident.
pub(crate) fn load_csr(
    dir: &Path,
    db_id: [u8; 16],
    expected_gen: u64,
    want_weights: bool,
) -> Option<CsrSnapshot> {
    if !can_map_files() {
        return None;
    }
    let file = Arc::new(MappedFile::open(&csr_path(dir)).ok()?);
    let bytes = file.bytes();
    if bytes.len() < CSR_HEADER_LEN + 8 {
        return None;
    }
    if &bytes[..8] != MAGIC || bytes[8..24] != db_id {
        return None;
    }
    if u64_at(bytes, 24)? != expected_gen {
        return None;
    }
    let flags = u64_at(bytes, 32)?;
    let weighted = flags & FLAG_WEIGHTED != 0;
    if want_weights && !weighted {
        return None;
    }
    let n64 = u64_at(bytes, 40)?;
    let e64 = u64_at(bytes, 48)?;
    // Before anything is served: the claimed lengths must describe exactly the
    // bytes the file has.
    if csr_file_len(n64, e64, weighted)? != bytes.len() as u64 {
        return None;
    }
    let body_len = bytes.len() - 8;
    if checksum_of(&bytes[..body_len]) != u64_at(bytes, body_len)? {
        return None;
    }
    let n = usize::try_from(n64).ok()?;
    let e = usize::try_from(e64).ok()?;
    if e > crate::csr::MAX_SNAPSHOT_EDGES {
        return None;
    }

    let mut offset = CSR_HEADER_LEN;
    let mut take = |len: usize, width: usize| -> Option<usize> {
        let at = offset;
        offset = offset.checked_add(len.checked_mul(width)?)?;
        Some(at)
    };
    let dense_to_id: Array<NodeId> = Array::mapped(&file, take(n, 8)?, n)?;
    let row_ptr: Array<usize> = Array::mapped(&file, take(n + 1, 8)?, n + 1)?;
    let col_idx: Array<u32> = Array::mapped(&file, take(e, 4)?, e)?;
    let edge_type: Array<TypeId> = Array::mapped(&file, take(e, 4)?, e)?;
    let edge_id: Array<EdgeId> = Array::mapped(&file, take(e, 8)?, e)?;
    let in_row_ptr: Array<usize> = Array::mapped(&file, take(n + 1, 8)?, n + 1)?;
    let in_col_idx: Array<u32> = Array::mapped(&file, take(e, 4)?, e)?;
    let in_edge_type: Array<TypeId> = Array::mapped(&file, take(e, 4)?, e)?;
    let in_pos: Array<u32> = Array::mapped(&file, take(e, 4)?, e)?;
    // Five 4-byte arrays leave the offset 4 bytes short of a multiple of 8 when
    // `e` is odd; the weights are 8-byte values, so realign before them.
    let edge_weight = if weighted {
        take(weight_padding(e64), 1)?;
        Some(Array::<f64>::mapped(&file, take(e, 8)?, e)?)
    } else {
        None
    };
    debug_assert_eq!(offset, body_len);

    let id_to_dense = dense_to_id
        .iter()
        .enumerate()
        .map(|(d, &id)| (id, d as u32))
        .collect();
    Some(CsrSnapshot {
        row_ptr,
        col_idx,
        edge_type,
        edge_id,
        edge_weight,
        has_negative_weight: flags & FLAG_NEGATIVE_WEIGHT != 0,
        in_row_ptr,
        in_col_idx,
        in_edge_type,
        in_pos,
        dense_to_id,
        id_to_dense,
    })
}

const COL_MAGIC: &[u8; 8] = b"ISSNCOL1";

/// Bytes of the fixed columns header: magic, database identity, generation,
/// entity count, column count, and the directory length.
const COL_HEADER_LEN: usize = 8 + 16 + 8 + 8 + 8 + 8;

/// Column kinds as stored in the directory.
const KIND_INT: u8 = 0;
const KIND_FLOAT: u8 = 1;
const KIND_BOOL: u8 = 2;
const KIND_STR: u8 = 3;
const KIND_JSON: u8 = 4;

/// One column's entry in the directory: its arrays as `(offset, length)`
/// pairs, offsets relative to the start of the data region and lengths in
/// elements. `Int` and `Float` hold the presence words and the values, `Bool`
/// the presence words and the byte values, `Str` the index, the dictionary
/// offsets, and the dictionary bytes, and `Json` one msgpack byte run.
#[derive(serde::Serialize, serde::Deserialize)]
struct ColumnEntry {
    name: String,
    kind: u8,
    parts: Vec<(u64, u64)>,
}

fn align8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

/// Lays out the data region: each array gets an 8-byte-aligned offset in the
/// order it will be written.
struct Layout {
    next: usize,
}

impl Layout {
    fn place(&mut self, byte_len: usize) -> u64 {
        let at = self.next;
        self.next = align8(at + byte_len);
        at as u64
    }
}

/// The generation an existing cache file claims, or `None` when there is no
/// readable header or the file belongs to another database. Lets a save skip
/// rewriting a file that already reflects the current generation, without a
/// foreign file's coincidental generation ever qualifying for the skip.
fn cache_file_gen(path: &Path, db_id: [u8; 16]) -> Option<u64> {
    let mut r = BufReader::new(File::open(path).ok()?);
    let mut header = [0u8; 32];
    r.read_exact(&mut header).ok()?;
    if &header[..8] != COL_MAGIC || header[8..24] != db_id {
        return None;
    }
    let mut gen_bytes = [0u8; 8];
    gen_bytes.copy_from_slice(&header[24..]);
    Some(u64::from_le_bytes(gen_bytes))
}

/// Persist the column set for `commit_gen`, atomically via temp file and
/// rename; a save whose file already claims `commit_gen` is skipped, so a
/// warm-up that materializes on every boot rewrites nothing while the graph
/// is unchanged.
///
/// The typed columns are written as their plain arrays at 8-byte-aligned
/// offsets, described by a directory the loader reads first, so a later
/// process maps the file and each column becomes a view of it; a column no
/// query touches never leaves the disk. The `Json` fallback columns are the
/// one heap-decoded part.
pub(crate) fn save_columns<S: crate::columns::ColumnSource<Id = u64>>(
    storage: &crate::storage::Storage,
    cols: &crate::columns::PropColumns<S>,
    commit_gen: u64,
) -> Result<(), Error> {
    use crate::columns::PropColumn;

    let dir = storage.env.path();
    let path = dir.join(S::CACHE_FILE);
    if cache_file_gen(&path, storage.db_id) == Some(commit_gen) {
        return Ok(());
    }
    let tmp = dir.join(format!("{}.tmp", S::CACHE_FILE));
    let (dense_to_id, col_list) = cols.cache_file_parts();

    // Lay out the data region and build the directory before writing anything,
    // since the directory's length decides where the data region starts.
    let mut layout = Layout { next: 0 };
    let dense_off = layout.place(dense_to_id.len() * 8);
    let mut entries = Vec::with_capacity(col_list.len());
    let mut json_blobs: Vec<Vec<u8>> = Vec::new();
    for (name, col) in &col_list {
        let (kind, parts) = match col {
            PropColumn::Int(v) => (
                KIND_INT,
                vec![
                    (
                        layout.place(v.present().words().len() * 8),
                        v.present().words().len() as u64,
                    ),
                    (layout.place(v.len() * 8), v.len() as u64),
                ],
            ),
            PropColumn::Float(v) => (
                KIND_FLOAT,
                vec![
                    (
                        layout.place(v.present().words().len() * 8),
                        v.present().words().len() as u64,
                    ),
                    (layout.place(v.len() * 8), v.len() as u64),
                ],
            ),
            PropColumn::Bool(v) => (
                KIND_BOOL,
                vec![
                    (
                        layout.place(v.present().words().len() * 8),
                        v.present().words().len() as u64,
                    ),
                    (layout.place(v.len()), v.len() as u64),
                ],
            ),
            PropColumn::Str { dict, idx } => (
                KIND_STR,
                vec![
                    (layout.place(idx.len() * 4), idx.len() as u64),
                    (
                        layout.place(dict.offsets().len() * 8),
                        dict.offsets().len() as u64,
                    ),
                    (layout.place(dict.bytes().len()), dict.bytes().len() as u64),
                ],
            ),
            PropColumn::Json(v) => {
                let blob = rmp_serde::to_vec(v)?;
                let part = (layout.place(blob.len()), blob.len() as u64);
                json_blobs.push(blob);
                (KIND_JSON, vec![part])
            }
        };
        entries.push(ColumnEntry {
            name: (*name).clone(),
            kind,
            parts,
        });
    }
    let directory = rmp_serde::to_vec(&entries)?;

    let write = || -> Result<(), Error> {
        let mut w = SumWriter {
            inner: BufWriter::new(File::create(&tmp).map_err(Error::Io)?),
            sum: Sum64::new(),
        };
        let io = Error::Io;
        w.put(COL_MAGIC).map_err(io)?;
        w.put(&storage.db_id).map_err(io)?;
        w.put_u64(commit_gen).map_err(io)?;
        w.put_u64(dense_to_id.len() as u64).map_err(io)?;
        w.put_u64(col_list.len() as u64).map_err(io)?;
        w.put_u64(directory.len() as u64).map_err(io)?;
        w.put(&directory).map_err(io)?;
        w.pad_to_8(COL_HEADER_LEN + directory.len()).map_err(io)?;

        // The data region, in the layout's order; each array is padded to 8.
        let mut written = 0usize;
        let mut pad_after = |w: &mut SumWriter<BufWriter<File>>, byte_len: usize| {
            written += byte_len;
            let padded = align8(written);
            let pad = padded - written;
            written = padded;
            w.put(&[0u8; 8][..pad])
        };
        debug_assert_eq!(dense_off, 0);
        w.put_u64s(dense_to_id).map_err(io)?;
        pad_after(&mut w, dense_to_id.len() * 8).map_err(io)?;
        let mut json_blobs = json_blobs.iter();
        for (_, col) in &col_list {
            match col {
                PropColumn::Int(v) => {
                    w.put_u64s(v.present().words()).map_err(io)?;
                    pad_after(&mut w, v.present().words().len() * 8).map_err(io)?;
                    for &x in v.values().iter() {
                        w.put(&x.to_le_bytes()).map_err(io)?;
                    }
                    pad_after(&mut w, v.len() * 8).map_err(io)?;
                }
                PropColumn::Float(v) => {
                    w.put_u64s(v.present().words()).map_err(io)?;
                    pad_after(&mut w, v.present().words().len() * 8).map_err(io)?;
                    w.put_f64s(v.values()).map_err(io)?;
                    pad_after(&mut w, v.len() * 8).map_err(io)?;
                }
                PropColumn::Bool(v) => {
                    w.put_u64s(v.present().words()).map_err(io)?;
                    pad_after(&mut w, v.present().words().len() * 8).map_err(io)?;
                    w.put(v.values()).map_err(io)?;
                    pad_after(&mut w, v.len()).map_err(io)?;
                }
                PropColumn::Str { dict, idx } => {
                    w.put_u32s(idx).map_err(io)?;
                    pad_after(&mut w, idx.len() * 4).map_err(io)?;
                    w.put_u64s(dict.offsets()).map_err(io)?;
                    pad_after(&mut w, dict.offsets().len() * 8).map_err(io)?;
                    w.put(dict.bytes()).map_err(io)?;
                    pad_after(&mut w, dict.bytes().len()).map_err(io)?;
                }
                PropColumn::Json(_) => {
                    let Some(blob) = json_blobs.next() else {
                        return Err(Error::Corrupt("columns save: a Json column has no blob"));
                    };
                    w.put(blob).map_err(io)?;
                    pad_after(&mut w, blob.len()).map_err(io)?;
                }
            }
        }
        debug_assert_eq!(written, layout.next);
        w.finish().map_err(io)
    };
    write()?;
    std::fs::rename(&tmp, path).map_err(Error::Io)?;
    Ok(())
}

/// Map the columns cache file if it exists and reflects storage's current
/// persisted generation. As with the CSR cache file, every refusal (missing,
/// stale, truncated, corrupt, or inconsistent file, or a target that cannot
/// map the format) means "scan instead" and never an error. The checksum is
/// verified over the whole file first; after that each typed column is a view
/// of the mapping and only the pages a query reads become resident.
pub(crate) fn load_columns<S: crate::columns::ColumnSource<Id = u64>>(
    storage: &crate::storage::Storage,
) -> Option<crate::columns::PropColumns<S>> {
    use crate::columns::{Bitmap, Nullable, PropColumn, StrDict};

    if !can_map_files() {
        return None;
    }
    let expected_gen = {
        let rtxn = storage.env.read_txn().ok()?;
        crate::storage::ids::commit_gen(storage, &rtxn).ok()?
    };
    let path = storage.env.path().join(S::CACHE_FILE);
    let file = Arc::new(MappedFile::open(&path).ok()?);
    let bytes = file.bytes();
    if bytes.len() < COL_HEADER_LEN + 8 {
        return None;
    }
    if &bytes[..8] != COL_MAGIC || bytes[8..24] != storage.db_id {
        return None;
    }
    if u64_at(bytes, 24)? != expected_gen {
        return None;
    }
    let body_len = bytes.len() - 8;
    if checksum_of(&bytes[..body_len]) != u64_at(bytes, body_len)? {
        return None;
    }
    let n = usize::try_from(u64_at(bytes, 32)?).ok()?;
    let ncols = usize::try_from(u64_at(bytes, 40)?).ok()?;
    let dir_len = usize::try_from(u64_at(bytes, 48)?).ok()?;
    let dir_end = COL_HEADER_LEN.checked_add(dir_len)?;
    let entries: Vec<ColumnEntry> =
        rmp_serde::from_slice(bytes.get(COL_HEADER_LEN..dir_end)?).ok()?;
    if entries.len() != ncols {
        return None;
    }
    let data_start = align8(dir_end);
    // Every part must lie inside the data region, which ends at the checksum.
    let region_len = body_len.checked_sub(data_start)?;
    let part = |(off, len): (u64, u64), width: usize| -> Option<(usize, usize)> {
        let off = usize::try_from(off).ok()?;
        let len = usize::try_from(len).ok()?;
        let end = off.checked_add(len.checked_mul(width)?)?;
        (end <= region_len).then_some((data_start + off, len))
    };
    let mapped = |p: (u64, u64), width: usize| -> Option<(usize, usize)> { part(p, width) };

    let (dense_off, _) = mapped((0, n as u64), 8)?;
    let dense_to_id: Array<u64> = Array::mapped(&file, dense_off, n)?;

    let mut cols = Vec::with_capacity(entries.len());
    for entry in entries {
        let parts = entry.parts.as_slice();
        let col = match (entry.kind, parts) {
            (KIND_INT, [words, values]) => {
                let (wo, wl) = mapped(*words, 8)?;
                let (vo, vl) = mapped(*values, 8)?;
                let present = Bitmap::from_words(Array::mapped(&file, wo, wl)?, vl)?;
                PropColumn::Int(Nullable::from_parts(
                    present,
                    Array::<i64>::mapped(&file, vo, vl)?,
                )?)
            }
            (KIND_FLOAT, [words, values]) => {
                let (wo, wl) = mapped(*words, 8)?;
                let (vo, vl) = mapped(*values, 8)?;
                let present = Bitmap::from_words(Array::mapped(&file, wo, wl)?, vl)?;
                PropColumn::Float(Nullable::from_parts(
                    present,
                    Array::<f64>::mapped(&file, vo, vl)?,
                )?)
            }
            (KIND_BOOL, [words, values]) => {
                let (wo, wl) = mapped(*words, 8)?;
                let (vo, vl) = mapped(*values, 1)?;
                let present = Bitmap::from_words(Array::mapped(&file, wo, wl)?, vl)?;
                let values: Array<u8> = Array::mapped(&file, vo, vl)?;
                if values.iter().any(|&b| b > 1) {
                    return None;
                }
                PropColumn::Bool(Nullable::from_parts(present, values)?)
            }
            (KIND_STR, [idx, offsets, dict_bytes]) => {
                let (io, il) = mapped(*idx, 4)?;
                let (oo, ol) = mapped(*offsets, 8)?;
                let (bo, bl) = mapped(*dict_bytes, 1)?;
                let idx: Array<u32> = Array::mapped(&file, io, il)?;
                let dict = StrDict::from_parts(
                    Array::mapped(&file, oo, ol)?,
                    Array::mapped(&file, bo, bl)?,
                )?;
                if idx
                    .iter()
                    .any(|&i| i != crate::columns::STR_NULL && i as usize >= dict.len())
                {
                    return None;
                }
                PropColumn::Str { dict, idx }
            }
            (KIND_JSON, [blob]) => {
                let (bo, bl) = mapped(*blob, 1)?;
                let values: Vec<Option<serde_json::Value>> =
                    rmp_serde::from_slice(bytes.get(bo..bo + bl)?).ok()?;
                PropColumn::Json(values)
            }
            _ => return None,
        };
        cols.push((entry.name, col));
    }
    crate::columns::PropColumns::from_cache_file(dense_to_id, cols)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::Graph;

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    /// `rebuild_csr` persists the snapshot, and a fresh process (a second
    /// `Graph::open` on the same directory) serves adjacency out of the loaded
    /// cache file with the same answers the rebuild would give.
    #[test]
    fn rebuild_persists_and_a_reopen_loads() {
        let dir = TempDir::new().unwrap();
        let (a, b, c);
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            a = g.add_node("N", &json!({})).unwrap();
            b = g.add_node("N", &json!({})).unwrap();
            c = g.add_node("N", &json!({})).unwrap();
            g.add_edge(a, b, "R", &json!({})).unwrap();
            g.add_edge(b, c, "R", &json!({})).unwrap();
            g.rebuild_csr().unwrap();
        }
        assert!(csr_path(dir.path()).exists(), "rebuild_csr must persist");

        let g = Graph::open(dir.path(), 1).unwrap();
        // Uses the snapshot; a wrong or empty cache file load would miss the path.
        let path = g.shortest_path(a, c).unwrap().expect("a -> b -> c");
        assert_eq!(path, vec![a, b, c]);
    }

    /// A write after the save moves the persisted generation, so the cache file
    /// must be refused and the rebuild must see the new edge.
    #[test]
    fn a_stale_cache_file_is_refused() {
        let dir = TempDir::new().unwrap();
        let (a, b, c);
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            a = g.add_node("N", &json!({})).unwrap();
            b = g.add_node("N", &json!({})).unwrap();
            c = g.add_node("N", &json!({})).unwrap();
            g.add_edge(a, b, "R", &json!({})).unwrap();
            g.rebuild_csr().unwrap();
            // Lands after the save, so the cache file no longer reflects storage.
            g.add_edge(b, c, "R", &json!({})).unwrap();
        }
        let g = Graph::open(dir.path(), 1).unwrap();
        let path = g.shortest_path(a, c).unwrap();
        assert_eq!(
            path,
            Some(vec![a, b, c]),
            "the post-save edge must be visible, so the stale cache file must not serve"
        );
    }

    /// A corrupt cache file degrades to the ordinary rebuild.
    #[test]
    fn a_corrupt_cache_file_is_refused() {
        let dir = TempDir::new().unwrap();
        let (a, b);
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            a = g.add_node("N", &json!({})).unwrap();
            b = g.add_node("N", &json!({})).unwrap();
            g.add_edge(a, b, "R", &json!({})).unwrap();
            g.rebuild_csr().unwrap();
        }
        // Flip one payload byte past the header.
        let p = csr_path(dir.path());
        let mut bytes = std::fs::read(&p).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&p, bytes).unwrap();

        let g = Graph::open(dir.path(), 1).unwrap();
        let path = g.shortest_path(a, b).unwrap();
        assert_eq!(path, Some(vec![a, b]));
    }

    /// Materializing persists the columns, a reopen loads them without the
    /// scan, and every column kind survives the round trip exactly, the
    /// mixed-kind `Json` fallback included.
    #[test]
    fn materialize_persists_columns_and_a_reopen_loads_them() {
        use crate::columns::{ColumnSource, NodeSource};

        let dir = TempDir::new().unwrap();
        let ids: Vec<u64>;
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            ids = vec![
                g.add_node(
                    "N",
                    &json!({ "i": 42, "f": 1.5, "b": true, "s": "x", "m": 1 }),
                )
                .unwrap(),
                g.add_node("N", &json!({ "s": "y", "m": "one" })).unwrap(),
                g.add_node("N", &json!({ "i": 7, "m": [1, 2] })).unwrap(),
            ];
            g.materialize_property_columns().unwrap();
            assert!(dir.path().join(NodeSource::CACHE_FILE).exists());
        }

        let g = Graph::open(dir.path(), 1).unwrap();
        assert!(
            load_columns::<NodeSource>(&g.storage).is_some(),
            "a fresh cache file must load"
        );
        // Served through the cache's build-or-load arm: bulk reads answer
        // exactly what a scan-built set would.
        let vals = g
            .node_prop_json_column(&ids, "m")
            .expect("bulk gather through the loaded columns");
        assert_eq!(vals, vec![json!(1), json!("one"), json!([1, 2])]);
        let vals = g.node_prop_json_column(&ids, "s").unwrap();
        assert_eq!(vals, vec![json!("x"), json!("y"), serde_json::Value::Null]);
        // The rebuilt string interning table accepts a patch (an update lands
        // through the loaded columns without a rebuild).
        g.update_node(ids[2], &json!({ "s": "x" })).unwrap();
        assert_eq!(g.node_prop_json(ids[2], "s").unwrap(), Some(json!("x")));

        // A later write moves the generation, so the cache file is refused.
        g.add_node("N", &json!({ "i": 1 })).unwrap();
        assert!(
            load_columns::<NodeSource>(&g.storage).is_none(),
            "a stale columns cache file must be refused"
        );
    }

    /// The edge columns have the same persist-and-reload contract as the node
    /// columns, through their own file.
    #[test]
    fn materialize_persists_edge_columns_and_a_reopen_loads_them() {
        use crate::columns::{ColumnSource, EdgeSource};

        let dir = TempDir::new().unwrap();
        let e;
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            let a = g.add_node("N", &json!({})).unwrap();
            let b = g.add_node("N", &json!({})).unwrap();
            e = g.add_edge(a, b, "R", &json!({ "w": 2.5 })).unwrap();
            g.materialize_edge_property_columns().unwrap();
            assert!(dir.path().join(EdgeSource::CACHE_FILE).exists());
        }

        let g = Graph::open(dir.path(), 1).unwrap();
        assert!(
            load_columns::<EdgeSource>(&g.storage).is_some(),
            "a fresh edge columns cache file must load"
        );
        assert_eq!(
            g.edge_prop_json_column(&[e], "w").unwrap(),
            vec![json!(2.5)]
        );

        // A later write moves the generation, so the file is refused.
        g.add_node("N", &json!({})).unwrap();
        assert!(load_columns::<EdgeSource>(&g.storage).is_none());
    }

    /// A loaded column set views the mapped file: the typed arrays and the
    /// dense mapping are mapped, every kind reads back exactly, and the first
    /// patch copies the touched column onto the heap while the file and any
    /// other holder of the mapped set stay untouched.
    #[test]
    fn loaded_columns_are_mapped_and_patch_onto_the_heap() {
        use crate::columns::{ColumnSource, NodeSource, PropColumn};

        let dir = TempDir::new().unwrap();
        let ids: Vec<u64>;
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            ids = vec![
                g.add_node(
                    "N",
                    &json!({ "i": 1, "f": 0.5, "b": false, "s": "a", "m": 1 }),
                )
                .unwrap(),
                g.add_node("N", &json!({ "i": -7, "b": true, "s": "bb", "m": "x" }))
                    .unwrap(),
                g.add_node("N", &json!({ "f": 2.5, "s": "a" })).unwrap(),
            ];
            g.materialize_property_columns().unwrap();
        }
        let g = Graph::open(dir.path(), 1).unwrap();
        let loaded = load_columns::<NodeSource>(&g.storage).expect("a fresh file loads");
        assert!(loaded.dense_to_id.is_mapped());
        match &loaded.cols["i"] {
            PropColumn::Int(v) => {
                assert!(v.values().is_mapped() && v.present().words().is_mapped());
                assert_eq!(v.iter().collect::<Vec<_>>(), vec![Some(1), Some(-7), None]);
            }
            _ => panic!("i is an Int column"),
        }
        match &loaded.cols["f"] {
            PropColumn::Float(v) => {
                assert_eq!(
                    v.iter().collect::<Vec<_>>(),
                    vec![Some(0.5), None, Some(2.5)]
                );
            }
            _ => panic!("f is a Float column"),
        }
        match &loaded.cols["b"] {
            PropColumn::Bool(v) => {
                assert_eq!(v.iter().collect::<Vec<_>>(), vec![Some(0), Some(1), None]);
            }
            _ => panic!("b is a Bool column"),
        }
        match &loaded.cols["s"] {
            PropColumn::Str { dict, idx } => {
                assert!(idx.is_mapped() && dict.offsets().is_mapped() && dict.bytes().is_mapped());
                assert_eq!(dict.iter().collect::<Vec<_>>(), vec!["a", "bb"]);
                assert_eq!(&idx[..], &[0, 1, 0]);
            }
            _ => panic!("s is a Str column"),
        }
        assert!(matches!(&loaded.cols["m"], PropColumn::Json(_)));
        let file_before = std::fs::read(dir.path().join(NodeSource::CACHE_FILE)).unwrap();

        // The graph's own columns come from the same file; a write patches them
        // onto the heap, interning a new string into the mapped dictionary.
        g.update_node(ids[2], &json!({ "i": 3, "s": "ccc" }))
            .unwrap();
        assert_eq!(
            g.node_prop_json_column(&ids, "i").unwrap(),
            vec![json!(1), json!(-7), json!(3)]
        );
        assert_eq!(
            g.node_prop_json_column(&ids, "s").unwrap(),
            vec![json!("a"), json!("bb"), json!("ccc")]
        );
        assert_eq!(
            g.node_prop_json_column(&ids, "f").unwrap(),
            vec![json!(0.5), serde_json::Value::Null, serde_json::Value::Null]
        );
        assert_eq!(
            std::fs::read(dir.path().join(NodeSource::CACHE_FILE)).unwrap(),
            file_before,
            "a patch never writes through the mapping"
        );
        match &loaded.cols["s"] {
            PropColumn::Str { dict, .. } => {
                assert_eq!(dict.len(), 2, "the mapped set is immutable")
            }
            _ => unreachable!(),
        }
    }

    /// A columns file whose directory or arrays are damaged is refused, never
    /// served: a flipped payload byte fails the checksum, and a directory that
    /// points a column past the data region or lists a dictionary index out of
    /// range is rejected even with a matching checksum.
    #[test]
    fn a_damaged_columns_file_is_refused() {
        use crate::columns::{ColumnSource, NodeSource};

        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        g.add_node("N", &json!({ "s": "abc", "i": 4 })).unwrap();
        g.add_node("N", &json!({ "s": "de" })).unwrap();
        g.materialize_property_columns().unwrap();
        let path = dir.path().join(NodeSource::CACHE_FILE);
        let good = std::fs::read(&path).unwrap();
        assert!(load_columns::<NodeSource>(&g.storage).is_some());

        // A flipped byte in the data region.
        let mut bytes = good.clone();
        let mid = COL_HEADER_LEN + (bytes.len() - COL_HEADER_LEN) / 2;
        bytes[mid] ^= 0x40;
        std::fs::write(&path, &bytes).unwrap();
        assert!(load_columns::<NodeSource>(&g.storage).is_none(), "checksum");

        // A truncated file.
        std::fs::write(&path, &good[..good.len() - 16]).unwrap();
        assert!(
            load_columns::<NodeSource>(&g.storage).is_none(),
            "truncated"
        );

        // A directory pointing past the data region, re-checksummed so only the
        // bounds check can catch it.
        let dir_len = u64_at(&good, 48).unwrap() as usize;
        let mut entries: Vec<ColumnEntry> =
            rmp_serde::from_slice(&good[COL_HEADER_LEN..COL_HEADER_LEN + dir_len]).unwrap();
        for e in &mut entries {
            for part in &mut e.parts {
                part.0 += 1 << 20;
            }
        }
        let bad_dir = rmp_serde::to_vec(&entries).unwrap();
        // Reassemble the file around the longer directory: header with the new
        // directory length, the directory padded to 8, the original data
        // region, and a fresh checksum.
        let data_start = align8(COL_HEADER_LEN + dir_len);
        let mut bytes = good[..48].to_vec();
        bytes.extend_from_slice(&(bad_dir.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&bad_dir);
        bytes.resize(align8(bytes.len()), 0);
        bytes.extend_from_slice(&good[data_start..good.len() - 8]);
        let sum = checksum_of(&bytes);
        bytes.extend_from_slice(&sum.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            load_columns::<NodeSource>(&g.storage).is_none(),
            "out of bounds"
        );

        // The good file still loads, and the graph answers through it.
        std::fs::write(&path, &good).unwrap();
        assert!(load_columns::<NodeSource>(&g.storage).is_some());
    }

    /// A repeated materialize at an unchanged generation must not rewrite the
    /// file.
    #[test]
    fn materialize_at_an_unchanged_generation_skips_the_rewrite() {
        use crate::columns::{ColumnSource, NodeSource};

        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        g.add_node("N", &json!({ "i": 1 })).unwrap();
        g.materialize_property_columns().unwrap();
        let path = dir.path().join(NodeSource::CACHE_FILE);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        g.materialize_property_columns().unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "an unchanged generation must skip the save");
    }

    /// A corrupt length field must refuse the load rather than feed the file's
    /// claim into `Vec::with_capacity`, where `u64::MAX` panics with "capacity
    /// overflow" and 2^40 aborts the process on allocation failure. The graph
    /// then answers from the ordinary rebuild.
    #[test]
    fn a_corrupt_length_field_is_refused_without_allocating() {
        let byte_offset_of_n = MAGIC.len() + 16 + 8 + 8;
        for corrupt_len in [u64::MAX, 1u64 << 40] {
            let dir = TempDir::new().unwrap();
            let (a, b);
            {
                let g = Graph::open(dir.path(), 1).unwrap();
                a = g.add_node("N", &json!({})).unwrap();
                b = g.add_node("N", &json!({})).unwrap();
                g.add_edge(a, b, "R", &json!({})).unwrap();
                g.rebuild_csr().unwrap();
            }
            let p = csr_path(dir.path());
            let mut bytes = std::fs::read(&p).unwrap();
            bytes[byte_offset_of_n..byte_offset_of_n + 8]
                .copy_from_slice(&corrupt_len.to_le_bytes());
            std::fs::write(&p, bytes).unwrap();

            let g = Graph::open(dir.path(), 1).unwrap();
            let path = g.shortest_path(a, b).unwrap();
            assert_eq!(path, Some(vec![a, b]), "length {corrupt_len}");
        }
    }

    /// A cache file left behind by one database must not serve another whose
    /// persisted generation happens to match, which is what a restore into a
    /// directory with leftover cache files produces. The file carries the
    /// database identity, so the foreign file is refused and adjacency comes
    /// from storage.
    #[test]
    fn a_cache_file_from_another_database_is_refused() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let (a, b, c);
        {
            // Five commits: three nodes, one edge, one dummy update.
            let g = Graph::open(dir1.path(), 1).unwrap();
            a = g.add_node("N", &json!({})).unwrap();
            b = g.add_node("N", &json!({})).unwrap();
            c = g.add_node("N", &json!({})).unwrap();
            g.add_edge(a, b, "R", &json!({})).unwrap();
            g.update_node(a, &json!({ "x": 1 })).unwrap();
            g.rebuild_csr().unwrap();
        }
        {
            // Five commits as well, so both databases sit at one persisted
            // generation, but the adjacency differs: a path a -> b -> c exists
            // here and not in the first database.
            let g = Graph::open(dir2.path(), 1).unwrap();
            let a2 = g.add_node("N", &json!({})).unwrap();
            let b2 = g.add_node("N", &json!({})).unwrap();
            let c2 = g.add_node("N", &json!({})).unwrap();
            g.add_edge(a2, b2, "R", &json!({})).unwrap();
            g.add_edge(b2, c2, "R", &json!({})).unwrap();
            assert_eq!((a2, b2, c2), (a, b, c));
        }
        // The second database's file lands in the first one's directory, with
        // the first one's cache file still there.
        std::fs::remove_file(dir1.path().join("data.mdb")).unwrap();
        let _ = std::fs::remove_file(dir1.path().join("lock.mdb"));
        std::fs::copy(dir2.path().join("data.mdb"), dir1.path().join("data.mdb")).unwrap();

        let g = Graph::open(dir1.path(), 1).unwrap();
        assert_eq!(
            g.shortest_path(a, c).unwrap(),
            Some(vec![a, b, c]),
            "the foreign cache file must not serve the old database's adjacency"
        );
    }

    /// `Graph::rebuild_csr` is the save site, so it must rebuild from storage
    /// rather than load the file it is about to overwrite. A wrong file that
    /// claims the current generation could otherwise never be repaired.
    #[test]
    fn rebuild_csr_rebuilds_from_storage_not_the_cache_file() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({})).unwrap();
        let stale_snap = CsrSnapshot::build(&g.storage).unwrap();
        g.add_edge(b, c, "R", &json!({})).unwrap();
        // A file that claims the current generation while missing the last
        // edge, which is what a torn or misdirected save would leave behind.
        let persisted_gen = {
            let rtxn = g.storage.env.read_txn().unwrap();
            crate::storage::ids::commit_gen(&g.storage, &rtxn).unwrap()
        };
        save_csr(dir.path(), &stale_snap, g.storage.db_id, persisted_gen).unwrap();

        g.rebuild_csr().unwrap();
        assert_eq!(
            g.shortest_path(a, c).unwrap(),
            Some(vec![a, b, c]),
            "rebuild_csr must not answer from the cache file"
        );

        // The rebuild also repaired the file: a fresh process loads the fixed
        // arrays and sees the edge.
        drop(g);
        let g = Graph::open(dir.path(), 1).unwrap();
        assert_eq!(g.shortest_path(a, c).unwrap(), Some(vec![a, b, c]));
    }

    /// A loaded snapshot views the mapped file rather than copying it, answers
    /// exactly what a build from storage answers, and the first patch after a
    /// write copies onto the heap while the mapped pages stay untouched. The
    /// file may be replaced under a live mapping (a later `rebuild_csr` does
    /// exactly that), so the old mapping must keep serving.
    #[test]
    fn a_loaded_snapshot_maps_the_file_and_patches_onto_the_heap() {
        let dir = TempDir::new().unwrap();
        let (a, b, c);
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            a = g.add_node("N", &json!({})).unwrap();
            b = g.add_node("N", &json!({})).unwrap();
            c = g.add_node("N", &json!({})).unwrap();
            g.add_edge(a, b, "R", &json!({})).unwrap();
            g.add_edge(c, b, "R", &json!({})).unwrap();
            g.rebuild_csr().unwrap();
        }
        let g = Graph::open(dir.path(), 1).unwrap();
        g.ensure_snapshot_fresh().unwrap();
        let loaded = g.csr_cache.snapshot.load_full();
        assert!(loaded.col_idx.is_mapped(), "the load must map, not copy");
        assert!(loaded.row_ptr.is_mapped());
        assert!(loaded.in_pos.is_mapped());
        assert!(loaded.dense_to_id.is_mapped());
        let built = CsrSnapshot::build(&g.storage).unwrap();
        assert_eq!(loaded.col_idx, built.col_idx);
        assert_eq!(loaded.in_col_idx, built.in_col_idx);
        assert_eq!(loaded.in_pos, built.in_pos);
        assert_eq!(loaded.id_to_dense, built.id_to_dense);

        // A write is absorbed by the incremental refresh: the patched snapshot is
        // owned, the mapped one is unchanged, and both read correctly.
        let e = g.add_edge(b, c, "R", &json!({})).unwrap();
        g.ensure_snapshot_fresh().unwrap();
        let patched = g.csr_cache.snapshot.load_full();
        assert!(!patched.col_idx.is_mapped(), "a patch copies onto the heap");
        assert_eq!(patched.col_idx.len(), 3);
        assert!(patched.edge_id.contains(&e));
        assert_eq!(loaded.col_idx.len(), 2, "the mapped snapshot is immutable");
        let first_in_of_c = patched.in_row_ptr[patched.id_to_dense[&c] as usize];
        assert_eq!(patched.edge_id[patched.in_pos[first_in_of_c] as usize], e);

        // Replacing the file under the live mapping (what `rebuild_csr` does)
        // must not disturb it.
        g.rebuild_csr().unwrap();
        assert_eq!(loaded.col_idx.len(), 2);
        assert_eq!(loaded.dense_to_id[0], a);
        assert_eq!(g.shortest_path(a, c).unwrap(), Some(vec![a, b, c]));
    }

    /// The word checksum is independent of how the bytes are chunked, covers a
    /// partial trailing word, and changes when any byte does.
    #[test]
    fn the_checksum_is_chunking_independent_and_sensitive() {
        let data: Vec<u8> = (0..37u8).collect();
        let whole = checksum_of(&data);
        let mut chunked = Sum64::new();
        chunked.update(&data[..3]);
        chunked.update(&data[3..20]);
        chunked.update(&data[20..]);
        assert_eq!(chunked.finish(), whole);
        for i in 0..data.len() {
            let mut flipped = data.clone();
            flipped[i] ^= 0x01;
            assert_ne!(checksum_of(&flipped), whole, "byte {i}");
        }
        let mut longer = data.clone();
        longer.push(0);
        assert_ne!(
            checksum_of(&longer),
            whole,
            "a trailing zero changes the sum"
        );
    }

    /// The exact arrays survive a save and load, weights included.
    #[test]
    fn arrays_round_trip_exactly() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({ "weight": 2.5 })).unwrap();
        g.add_edge(b, c, "S", &json!({ "weight": -1.0 })).unwrap();
        g.add_edge(a, c, "R", &json!({})).unwrap();

        let snap = CsrSnapshot::build_weighted(&g.storage).unwrap();
        let out = TempDir::new().unwrap();
        let db_id = g.storage.db_id;
        save_csr(out.path(), &snap, db_id, 7).unwrap();

        assert!(
            load_csr(out.path(), db_id, 8, false).is_none(),
            "wrong generation"
        );
        assert!(
            load_csr(out.path(), [0xAB; 16], 7, false).is_none(),
            "wrong database identity"
        );
        let loaded = load_csr(out.path(), db_id, 7, true).expect("fresh and weighted");
        assert_eq!(loaded.row_ptr, snap.row_ptr);
        assert_eq!(loaded.col_idx, snap.col_idx);
        assert_eq!(loaded.edge_type, snap.edge_type);
        assert_eq!(loaded.edge_id, snap.edge_id);
        assert_eq!(loaded.edge_weight, snap.edge_weight);
        assert_eq!(loaded.has_negative_weight, snap.has_negative_weight);
        assert!(loaded.has_negative_weight);
        assert_eq!(loaded.in_row_ptr, snap.in_row_ptr);
        assert_eq!(loaded.in_col_idx, snap.in_col_idx);
        assert_eq!(loaded.in_edge_type, snap.in_edge_type);
        assert_eq!(loaded.in_pos, snap.in_pos);
        assert_eq!(loaded.dense_to_id, snap.dense_to_id);
        assert_eq!(loaded.id_to_dense, snap.id_to_dense);

        // An unweighted ask accepts a weighted cache file.
        assert!(load_csr(out.path(), db_id, 7, false).is_some());
    }
}
