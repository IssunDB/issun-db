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
//! The CSR format is little-endian: a magic tag, the generation, flags, the
//! two array lengths, the arrays themselves in a fixed order, and a 64-bit
//! checksum folded over every preceding byte. The `id_to_dense` map is not
//! stored; it is rebuilt from `dense_to_id` on load. The columns files carry
//! a msgpack payload behind the same header and checksum discipline.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::csr::CsrSnapshot;
use crate::error::Error;

const MAGIC: &[u8; 8] = b"ISSNCSR1";
const FLAG_WEIGHTED: u64 = 1;
const FLAG_NEGATIVE_WEIGHT: u64 = 2;

/// FNV-1a folded over bytes. Corruption detection, not cryptography.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// Checksumming writer wrapper.
struct SumWriter<W: Write> {
    inner: W,
    sum: Fnv,
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
}

/// Checksumming reader wrapper. Every getter reads exactly its width, so a
/// truncated file surfaces as an `io` error the caller maps to "no cache file".
struct SumReader<R: Read> {
    inner: R,
    sum: Fnv,
}

impl<R: Read> SumReader<R> {
    fn get<const N: usize>(&mut self) -> std::io::Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.inner.read_exact(&mut buf)?;
        self.sum.update(&buf);
        Ok(buf)
    }

    fn get_u64(&mut self) -> std::io::Result<u64> {
        Ok(u64::from_le_bytes(self.get::<8>()?))
    }

    fn get_u64s(&mut self, n: usize) -> std::io::Result<Vec<u64>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.get_u64()?);
        }
        Ok(out)
    }

    fn get_usizes(&mut self, n: usize) -> std::io::Result<Vec<usize>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.get_u64()? as usize);
        }
        Ok(out)
    }

    fn get_u32s(&mut self, n: usize) -> std::io::Result<Vec<u32>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(u32::from_le_bytes(self.get::<4>()?));
        }
        Ok(out)
    }

    fn get_f64s(&mut self, n: usize) -> std::io::Result<Vec<f64>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(f64::from_le_bytes(self.get::<8>()?));
        }
        Ok(out)
    }
}

/// Streaming forms for the msgpack-encoded columns payload; each hashes
/// exactly the bytes that pass through.
impl<W: Write> Write for SumWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sum.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<R: Read> Read for SumReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.sum.update(&buf[..n]);
        Ok(n)
    }
}

fn csr_path(dir: &Path) -> PathBuf {
    dir.join("csr.cache")
}

/// Persist `snap` for `commit_gen`, atomically: the bytes go to a temp file in
/// the same directory and the rename publishes them, so a crash mid-write
/// leaves either the previous cache file or none, never a torn one.
pub(crate) fn save_csr(dir: &Path, snap: &CsrSnapshot, commit_gen: u64) -> Result<(), Error> {
    let tmp = dir.join("csr.cache.tmp");
    let write = || -> std::io::Result<()> {
        let mut w = SumWriter {
            inner: BufWriter::new(File::create(&tmp)?),
            sum: Fnv::new(),
        };
        w.put(MAGIC)?;
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
        w.put_u64s(&snap.in_edge_id)?;
        if let Some(weights) = &snap.edge_weight {
            w.put_f64s(weights)?;
        }
        let sum = w.sum.0;
        w.inner.write_all(&sum.to_le_bytes())?;
        w.inner.flush()?;
        Ok(())
    };
    write().map_err(Error::Io)?;
    std::fs::rename(&tmp, csr_path(dir)).map_err(Error::Io)?;
    Ok(())
}

/// Load the cache file if it exists and reflects `expected_gen`, carrying weights
/// when `want_weights` asks for them. `None` on a missing, stale, truncated,
/// corrupt, or version-mismatched file, and on an unweighted cache file when
/// weights are wanted. Every refusal means "build from storage instead", never
/// an error, because the file is a cache and storage can always answer.
pub(crate) fn load_csr(dir: &Path, expected_gen: u64, want_weights: bool) -> Option<CsrSnapshot> {
    let file = File::open(csr_path(dir)).ok()?;
    let mut r = SumReader {
        inner: BufReader::new(file),
        sum: Fnv::new(),
    };
    let read = |r: &mut SumReader<BufReader<File>>| -> std::io::Result<Option<CsrSnapshot>> {
        let magic = r.get::<8>()?;
        if &magic != MAGIC {
            return Ok(None);
        }
        let file_gen = r.get_u64()?;
        if file_gen != expected_gen {
            return Ok(None);
        }
        let flags = r.get_u64()?;
        let weighted = flags & FLAG_WEIGHTED != 0;
        if want_weights && !weighted {
            return Ok(None);
        }
        let n = r.get_u64()? as usize;
        let e = r.get_u64()? as usize;
        let dense_to_id = r.get_u64s(n)?;
        let row_ptr = r.get_usizes(n + 1)?;
        let col_idx = r.get_u32s(e)?;
        let edge_type = r.get_u32s(e)?;
        let edge_id = r.get_u64s(e)?;
        let in_row_ptr = r.get_usizes(n + 1)?;
        let in_col_idx = r.get_u32s(e)?;
        let in_edge_type = r.get_u32s(e)?;
        let in_edge_id = r.get_u64s(e)?;
        let edge_weight = if weighted { Some(r.get_f64s(e)?) } else { None };
        let expected_sum = r.sum.0;
        let mut sum_buf = [0u8; 8];
        r.inner.read_exact(&mut sum_buf)?;
        if u64::from_le_bytes(sum_buf) != expected_sum {
            return Ok(None);
        }
        let id_to_dense = dense_to_id
            .iter()
            .enumerate()
            .map(|(d, &id)| (id, d as u32))
            .collect();
        Ok(Some(CsrSnapshot {
            row_ptr,
            col_idx,
            edge_type,
            edge_id,
            edge_weight,
            has_negative_weight: flags & FLAG_NEGATIVE_WEIGHT != 0,
            in_row_ptr,
            in_col_idx,
            in_edge_type,
            in_edge_id,
            dense_to_id,
            id_to_dense,
        }))
    };
    read(&mut r).ok().flatten()
}

const COL_MAGIC: &[u8; 8] = b"ISSNCOL1";

/// The msgpack shape of a columns cache file payload; the borrowed form writes
/// and the owned form reads, so a save never clones a column.
#[derive(serde::Serialize)]
struct ColumnsPayloadRef<'a> {
    dense_to_id: &'a Vec<u64>,
    cols: Vec<(&'a String, &'a crate::columns::PropColumn)>,
}

#[derive(serde::Deserialize)]
struct ColumnsPayload {
    dense_to_id: Vec<u64>,
    cols: Vec<(String, crate::columns::PropColumn)>,
}

/// The generation an existing cache file file claims, or `None` when there is no
/// readable header. Lets a save skip rewriting a file that already reflects
/// the current generation.
fn cache_file_gen(path: &Path) -> Option<u64> {
    let mut r = BufReader::new(File::open(path).ok()?);
    let mut header = [0u8; 16];
    r.read_exact(&mut header).ok()?;
    if &header[..8] != COL_MAGIC {
        return None;
    }
    let mut gen_bytes = [0u8; 8];
    gen_bytes.copy_from_slice(&header[8..]);
    Some(u64::from_le_bytes(gen_bytes))
}

/// Persist the column set for `commit_gen`, atomically via temp file and
/// rename; a save whose file already claims `commit_gen` is skipped, so a
/// warm-up that materializes on every boot rewrites nothing while the graph
/// is unchanged.
pub(crate) fn save_columns<S: crate::columns::ColumnSource<Id = u64>>(
    storage: &crate::storage::Storage,
    cols: &crate::columns::PropColumns<S>,
    commit_gen: u64,
) -> Result<(), Error> {
    let dir = storage.env.path();
    let path = dir.join(S::CACHE_FILE);
    if cache_file_gen(&path) == Some(commit_gen) {
        return Ok(());
    }
    let tmp = dir.join(format!("{}.tmp", S::CACHE_FILE));
    let (dense_to_id, col_list) = cols.cache_file_parts();
    let payload = ColumnsPayloadRef {
        dense_to_id,
        cols: col_list,
    };
    let write = || -> Result<(), Error> {
        let mut w = SumWriter {
            inner: BufWriter::new(File::create(&tmp).map_err(Error::Io)?),
            sum: Fnv::new(),
        };
        w.put(COL_MAGIC).map_err(Error::Io)?;
        w.put_u64(commit_gen).map_err(Error::Io)?;
        rmp_serde::encode::write(&mut w, &payload)?;
        let sum = w.sum.0;
        w.inner.write_all(&sum.to_le_bytes()).map_err(Error::Io)?;
        w.inner.flush().map_err(Error::Io)?;
        Ok(())
    };
    write()?;
    std::fs::rename(&tmp, path).map_err(Error::Io)?;
    Ok(())
}

/// Load the columns cache file if it exists and reflects storage's current
/// persisted generation. As with the CSR cache file, every refusal (missing,
/// stale, truncated, corrupt, or inconsistent file) means "scan instead" and
/// never an error.
pub(crate) fn load_columns<S: crate::columns::ColumnSource<Id = u64>>(
    storage: &crate::storage::Storage,
) -> Option<crate::columns::PropColumns<S>> {
    let expected_gen = {
        let rtxn = storage.env.read_txn().ok()?;
        crate::storage::ids::commit_gen(storage, &rtxn).ok()?
    };
    let path = storage.env.path().join(S::CACHE_FILE);
    let mut r = SumReader {
        inner: BufReader::new(File::open(path).ok()?),
        sum: Fnv::new(),
    };
    let magic = r.get::<8>().ok()?;
    if &magic != COL_MAGIC {
        return None;
    }
    if r.get_u64().ok()? != expected_gen {
        return None;
    }
    let payload: ColumnsPayload = rmp_serde::decode::from_read(&mut r).ok()?;
    let expected_sum = r.sum.0;
    let mut sum_buf = [0u8; 8];
    r.inner.read_exact(&mut sum_buf).ok()?;
    if u64::from_le_bytes(sum_buf) != expected_sum {
        return None;
    }
    crate::columns::PropColumns::from_cache_file(payload.dense_to_id, payload.cols)
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
        save_csr(out.path(), &snap, 7).unwrap();

        assert!(load_csr(out.path(), 8, false).is_none(), "wrong generation");
        let loaded = load_csr(out.path(), 7, true).expect("fresh and weighted");
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
        assert_eq!(loaded.in_edge_id, snap.in_edge_id);
        assert_eq!(loaded.dense_to_id, snap.dense_to_id);
        assert_eq!(loaded.id_to_dense, snap.id_to_dense);

        // An unweighted ask accepts a weighted cache file.
        assert!(load_csr(out.path(), 7, false).is_some());
    }
}
