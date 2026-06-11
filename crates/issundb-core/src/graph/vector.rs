use super::*;

/// Meta key under which the vector search crate persists its per-graph index
/// configuration (metric and quantization). `issundb-core` owns the durable
/// `meta` record; the vector crate owns its byte encoding and semantics.
const VECTOR_CONFIG_KEY: &str = "vector_config";

impl Graph {
    // ------------------------------------------------------------------
    // Vector storage
    // ------------------------------------------------------------------

    /// Persist the vector index configuration bytes for this graph.
    ///
    /// The byte encoding is owned by the vector search crate; `issundb-core`
    /// only stores the opaque record under a fixed `meta` key.
    #[doc(hidden)]
    pub fn put_vector_config(&self, bytes: &[u8]) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.storage.meta.put(&mut wtxn, VECTOR_CONFIG_KEY, bytes)?;
        wtxn.commit()?;
        Ok(())
    }

    /// Return the persisted vector index configuration bytes, or `None` if the
    /// graph has never been configured.
    #[doc(hidden)]
    pub fn get_vector_config(&self) -> Result<Option<Vec<u8>>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        Ok(self
            .storage
            .meta
            .get(&rtxn, VECTOR_CONFIG_KEY)?
            .map(|b| b.to_vec()))
    }

    /// Persist raw vector bytes for `n`.
    ///
    /// Vector search crates own vector decoding, validation, and indexing.
    /// `issundb-core` only owns the durable LMDB record.
    #[doc(hidden)]
    pub fn put_vector_bytes(&self, n: NodeId, bytes: &[u8]) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.put_vector_bytes_impl(&mut wtxn, n, bytes)?;
        wtxn.commit()?;
        Ok(())
    }

    pub(super) fn put_vector_bytes_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        n: NodeId,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.storage.vectors.put(wtxn, &n, bytes)?;
        Ok(())
    }

    /// Delete the raw vector bytes for `n` from LMDB. No-op if absent.
    #[doc(hidden)]
    pub fn delete_vector_bytes(&self, n: NodeId) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.delete_vector_bytes_impl(&mut wtxn, n)?;
        wtxn.commit()?;
        Ok(())
    }

    pub(super) fn delete_vector_bytes_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        n: NodeId,
    ) -> Result<(), Error> {
        self.storage.vectors.delete(wtxn, &n)?;
        Ok(())
    }

    /// Return all raw vector records in node ID order.
    #[doc(hidden)]
    pub fn vector_bytes(&self) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.vector_bytes_impl(&rtxn)
    }

    pub(super) fn vector_bytes_impl(
        &self,
        rtxn: &heed::RoTxn,
    ) -> Result<Vec<(NodeId, Vec<u8>)>, Error> {
        let mut out = Vec::new();
        for result in self.storage.vectors.iter(rtxn)? {
            let (node_id, bytes) = result?;
            out.push((node_id, bytes.to_vec()));
        }
        Ok(out)
    }

    /// Return the raw vector bytes for `n`, or `None` if absent.
    #[doc(hidden)]
    pub fn get_vector_bytes(&self, n: NodeId) -> Result<Option<Vec<u8>>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.get_vector_bytes_impl(&rtxn, n)
    }

    pub(super) fn get_vector_bytes_impl(
        &self,
        rtxn: &heed::RoTxn,
        n: NodeId,
    ) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.storage.vectors.get(rtxn, &n)?.map(|b| b.to_vec()))
    }
}
