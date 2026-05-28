use super::*;

impl Graph {
    // ------------------------------------------------------------------
    // Vector storage
    // ------------------------------------------------------------------

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
}
