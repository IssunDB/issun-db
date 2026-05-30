use super::*;

impl Graph {
    #[doc(hidden)]
    pub fn create_node_text_index(&self, label: &str, property: &str) -> Result<(), Error> {
        self.create_node_text_index_with_language(label, property, Language::English)
    }

    #[doc(hidden)]
    pub fn create_node_text_index_with_language(
        &self,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.create_node_text_index_impl(&mut wtxn, label, property, lang)?;
        wtxn.commit()?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn drop_node_text_index(&self, label: &str, property: &str) -> Result<(), Error> {
        let _guard = self._write_lock.lock();
        let mut wtxn = self.storage.env.write_txn()?;
        self.drop_node_text_index_impl(&mut wtxn, label, property)?;
        wtxn.commit()?;
        Ok(())
    }

    #[doc(hidden)]
    pub fn has_node_text_index(&self, label: &str, property: &str) -> Result<bool, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.has_node_text_index_impl(&rtxn, label, property)
    }

    #[doc(hidden)]
    pub fn active_text_indexes(&self) -> Result<Vec<(String, String, Language)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.active_text_indexes_impl(&rtxn)
    }

    /// Tokenize `text` using the stemmer and stop-word rules for `lang`,
    /// returning a map from term to term-frequency count.
    ///
    /// Exposed on `Graph` so that higher-level crates (`issundb-text`) can
    /// perform BM25 scoring without reaching into `storage::fts` directly.
    #[doc(hidden)]
    pub fn tokenize_text(
        &self,
        text: &str,
        lang: Language,
    ) -> std::collections::HashMap<String, u32> {
        fts::tokenize(text, lang)
    }

    pub(super) fn active_text_indexes_impl(
        &self,
        rtxn: &heed::RoTxn,
    ) -> Result<Vec<(String, String, Language)>, Error> {
        let prefix = "fts_meta:node:l:";
        let mut results = Vec::new();
        for entry in self.storage.meta.prefix_iter(rtxn, prefix)? {
            let (key, val) = entry?;
            if let Some(suffix) = key.strip_prefix(prefix) {
                let parts: Vec<&str> = suffix.split(":p:").collect();
                if parts.len() == 2 {
                    if let (Ok(label_id), Ok(prop_key_id)) =
                        (parts[0].parse::<LabelId>(), parts[1].parse::<PropKeyId>())
                    {
                        if let (Some(label), Some(property)) = (
                            self.label_name_impl(rtxn, label_id)?,
                            self.prop_key_name_impl(rtxn, prop_key_id)?,
                        ) {
                            let lang = if !val.is_empty() {
                                Language::from_u8(val[0])
                            } else {
                                Language::English
                            };
                            results.push((label, property, lang));
                        }
                    }
                }
            }
        }
        Ok(results)
    }

    pub(super) fn get_fts_index_language(
        &self,
        rtxn: &heed::RoTxn,
        label_id: LabelId,
        prop_key_id: PropKeyId,
    ) -> Result<Language, Error> {
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");
        if let Some(bytes) = self.storage.meta.get(rtxn, &meta_key)? {
            if !bytes.is_empty() {
                return Ok(Language::from_u8(bytes[0]));
            }
        }
        Ok(Language::English)
    }

    pub(super) fn has_node_text_index_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
    ) -> Result<bool, Error> {
        let label_id = match get_label(&self.storage, rtxn, label)? {
            Some(id) => id,
            None => return Ok(false),
        };
        let prop_key_id = match get_prop_key(&self.storage, rtxn, property)? {
            Some(id) => id,
            None => return Ok(false),
        };
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");
        let active = self.storage.meta.get(rtxn, &meta_key)?.is_some();
        Ok(active)
    }

    pub(super) fn create_node_text_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        property: &str,
        lang: Language,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");

        if self.storage.meta.get(wtxn, &meta_key)?.is_some() {
            return Ok(());
        }

        let node_ids = self.nodes_by_label_impl(wtxn, label)?;

        let mut fts_n: u64 = 0;
        let mut fts_sum_dl: u64 = 0;

        for &node_id in &node_ids {
            let record = self
                .get_node_impl(wtxn, node_id)?
                .ok_or(Error::NodeNotFound(node_id))?;
            let props_json: serde_json::Value = props::decode(&record.props)?;
            if let Some(serde_json::Value::String(text_val)) = props_json.get(property) {
                let terms = fts::tokenize(text_val, lang);
                let doc_len: u32 = terms.values().sum();
                if doc_len > 0 {
                    let d_key = fts_doc_key(label_id, prop_key_id, node_id);
                    self.storage
                        .fts_docs
                        .put(wtxn, &d_key, &doc_len.to_be_bytes())?;

                    for (term, freq) in terms {
                        let p_key = fts_postings_key(label_id, prop_key_id, &term);
                        let p_val = fts_posting_val(node_id, freq);
                        self.storage.fts_postings.put(wtxn, &p_key, &p_val)?;
                    }

                    fts_n += 1;
                    fts_sum_dl += doc_len as u64;
                }
            }
        }

        self.storage.meta.put(wtxn, &meta_key, &[lang.to_u8()])?;
        self.storage.meta.put(
            wtxn,
            &fts_stats_n_key(label_id, prop_key_id),
            &fts_n.to_be_bytes(),
        )?;
        self.storage.meta.put(
            wtxn,
            &fts_stats_sum_dl_key(label_id, prop_key_id),
            &fts_sum_dl.to_be_bytes(),
        )?;

        Ok(())
    }

    pub(super) fn drop_node_text_index_impl(
        &self,
        wtxn: &mut heed::RwTxn,
        label: &str,
        property: &str,
    ) -> Result<(), Error> {
        let label_id = get_or_create_label(&self.storage, wtxn, label)?;
        let prop_key_id = get_or_create_prop_key(&self.storage, wtxn, property)?;
        let meta_key = format!("fts_meta:node:l:{label_id}:p:{prop_key_id}");

        if self.storage.meta.get(wtxn, &meta_key)?.is_some() {
            self.storage.meta.delete(wtxn, &meta_key)?;
            self.storage
                .meta
                .delete(wtxn, &fts_stats_n_key(label_id, prop_key_id))?;
            self.storage
                .meta
                .delete(wtxn, &fts_stats_sum_dl_key(label_id, prop_key_id))?;

            // Delete postings prefix
            let mut postings_prefix = Vec::with_capacity(8);
            postings_prefix.extend_from_slice(&label_id.to_be_bytes());
            postings_prefix.extend_from_slice(&prop_key_id.to_be_bytes());

            let mut postings_to_delete = Vec::new();
            for entry in self
                .storage
                .fts_postings
                .prefix_iter(wtxn, &postings_prefix)?
            {
                let (key, _) = entry?;
                postings_to_delete.push(key.to_vec());
            }
            for key in postings_to_delete {
                self.storage.fts_postings.delete(wtxn, &key)?;
            }

            // Delete doc lengths prefix
            let mut docs_prefix = Vec::with_capacity(8);
            docs_prefix.extend_from_slice(&label_id.to_be_bytes());
            docs_prefix.extend_from_slice(&prop_key_id.to_be_bytes());

            let mut docs_to_delete = Vec::new();
            for entry in self.storage.fts_docs.prefix_iter(wtxn, &docs_prefix)? {
                let (key, _) = entry?;
                docs_to_delete.push(key.to_vec());
            }
            for key in docs_to_delete {
                self.storage.fts_docs.delete(wtxn, &key)?;
            }
        }

        Ok(())
    }

    pub(super) fn active_text_indexes_for_label(
        &self,
        rtxn: &heed::RoTxn,
        label_id: LabelId,
    ) -> Result<Vec<PropKeyId>, Error> {
        let prefix = format!("fts_meta:node:l:{label_id}:p:");
        let mut prop_keys = Vec::new();
        for entry in self.storage.meta.prefix_iter(rtxn, &prefix)? {
            let (key, _) = entry?;
            if let Some(suffix) = key.strip_prefix(&prefix) {
                if let Ok(prop_key_id) = suffix.parse::<PropKeyId>() {
                    prop_keys.push(prop_key_id);
                }
            }
        }
        Ok(prop_keys)
    }

    pub(super) fn index_node_fts(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        props_json: &serde_json::Value,
    ) -> Result<(), Error> {
        // Collect read-side data before taking a mutable borrow.
        let adds: Vec<(PropKeyId, String)> = {
            let rtxn: &heed::RoTxn = wtxn;
            let active_prop_keys = self.active_text_indexes_for_label(rtxn, label_id)?;
            let mut out = Vec::new();
            for prop_key_id in active_prop_keys {
                if let Some(prop_name) = get_prop_key_name(&self.storage, rtxn, prop_key_id)? {
                    if let Some(serde_json::Value::String(text_val)) = props_json.get(&prop_name) {
                        out.push((prop_key_id, text_val.clone()));
                    }
                }
            }
            out
        };
        for (prop_key_id, text_val) in adds {
            self.add_node_prop_fts_entry(wtxn, node_id, label_id, prop_key_id, &text_val)?;
        }
        Ok(())
    }

    pub(super) fn delete_node_fts(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        props_json: &serde_json::Value,
    ) -> Result<(), Error> {
        // Collect read-side data before taking a mutable borrow.
        let removes: Vec<(PropKeyId, String)> = {
            let rtxn: &heed::RoTxn = wtxn;
            let active_prop_keys = self.active_text_indexes_for_label(rtxn, label_id)?;
            let mut out = Vec::new();
            for prop_key_id in active_prop_keys {
                if let Some(prop_name) = get_prop_key_name(&self.storage, rtxn, prop_key_id)? {
                    if let Some(serde_json::Value::String(text_val)) = props_json.get(&prop_name) {
                        out.push((prop_key_id, text_val.clone()));
                    }
                }
            }
            out
        };
        for (prop_key_id, text_val) in removes {
            self.remove_node_prop_fts_entry(wtxn, node_id, label_id, prop_key_id, &text_val)?;
        }
        Ok(())
    }

    pub(super) fn add_node_prop_fts_entry(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        prop_key_id: PropKeyId,
        text: &str,
    ) -> Result<(), Error> {
        let lang = {
            let rtxn: &heed::RoTxn = wtxn;
            self.get_fts_index_language(rtxn, label_id, prop_key_id)?
        };
        let terms = fts::tokenize(text, lang);
        let doc_len: u32 = terms.values().sum();
        if doc_len > 0 {
            let d_key = fts_doc_key(label_id, prop_key_id, node_id);
            self.storage
                .fts_docs
                .put(wtxn, &d_key, &doc_len.to_be_bytes())?;

            for (term, freq) in terms {
                let p_key = fts_postings_key(label_id, prop_key_id, &term);
                let p_val = fts_posting_val(node_id, freq);
                self.storage.fts_postings.put(wtxn, &p_key, &p_val)?;
            }

            // Stats
            let n_key = fts_stats_n_key(label_id, prop_key_id);
            let sum_dl_key = fts_stats_sum_dl_key(label_id, prop_key_id);

            let current_n = self
                .storage
                .meta
                .get(wtxn, &n_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats n: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);
            let current_sum_dl = self
                .storage
                .meta
                .get(wtxn, &sum_dl_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats sum_dl: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);

            self.storage
                .meta
                .put(wtxn, &n_key, &(current_n + 1).to_be_bytes())?;
            self.storage.meta.put(
                wtxn,
                &sum_dl_key,
                &(current_sum_dl + doc_len as u64).to_be_bytes(),
            )?;
        }
        Ok(())
    }

    pub(super) fn remove_node_prop_fts_entry(
        &self,
        wtxn: &mut heed::RwTxn,
        node_id: NodeId,
        label_id: LabelId,
        prop_key_id: PropKeyId,
        text: &str,
    ) -> Result<(), Error> {
        let d_key = fts_doc_key(label_id, prop_key_id, node_id);
        if let Some(bytes) = self.storage.fts_docs.get(wtxn, &d_key)? {
            let doc_len = parse_fts_doc_val(bytes)?;
            self.storage.fts_docs.delete(wtxn, &d_key)?;

            let lang = {
                let rtxn: &heed::RoTxn = wtxn;
                self.get_fts_index_language(rtxn, label_id, prop_key_id)?
            };
            let terms = fts::tokenize(text, lang);
            for (term, freq) in terms {
                let p_key = fts_postings_key(label_id, prop_key_id, &term);
                let p_val = fts_posting_val(node_id, freq);
                self.storage
                    .fts_postings
                    .delete_one_duplicate(wtxn, &p_key, &p_val)?;
            }

            // Stats
            let n_key = fts_stats_n_key(label_id, prop_key_id);
            let sum_dl_key = fts_stats_sum_dl_key(label_id, prop_key_id);

            let current_n = self
                .storage
                .meta
                .get(wtxn, &n_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats n: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);
            let current_sum_dl = self
                .storage
                .meta
                .get(wtxn, &sum_dl_key)?
                .map(|b| -> Result<u64, Error> {
                    Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                        Error::Corrupt("fts stats sum_dl: wrong byte length")
                    })?))
                })
                .transpose()?
                .unwrap_or(0);

            let next_n = current_n.saturating_sub(1);
            let next_sum_dl = current_sum_dl.saturating_sub(doc_len as u64);

            self.storage.meta.put(wtxn, &n_key, &next_n.to_be_bytes())?;
            self.storage
                .meta
                .put(wtxn, &sum_dl_key, &next_sum_dl.to_be_bytes())?;
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn fts_stats(&self, label: &str, property: &str) -> Result<Option<(u64, u64)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.fts_stats_impl(&rtxn, label, property)
    }

    pub(super) fn fts_stats_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
    ) -> Result<Option<(u64, u64)>, Error> {
        if !self.has_node_text_index_impl(rtxn, label, property)? {
            return Ok(None);
        }
        let label_id = get_label(&self.storage, rtxn, label)?
            .ok_or(Error::Corrupt("fts_stats: label id not found"))?;
        let prop_key_id = get_prop_key(&self.storage, rtxn, property)?
            .ok_or(Error::Corrupt("fts_stats: prop key id not found"))?;

        let n_key = fts_stats_n_key(label_id, prop_key_id);
        let sum_dl_key = fts_stats_sum_dl_key(label_id, prop_key_id);

        let n = self
            .storage
            .meta
            .get(rtxn, &n_key)?
            .map(|b| -> Result<u64, Error> {
                Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                    Error::Corrupt("fts stats n: wrong byte length")
                })?))
            })
            .transpose()?
            .unwrap_or(0);
        let sum_dl = self
            .storage
            .meta
            .get(rtxn, &sum_dl_key)?
            .map(|b| -> Result<u64, Error> {
                Ok(u64::from_be_bytes(b.try_into().map_err(|_| {
                    Error::Corrupt("fts stats sum_dl: wrong byte length")
                })?))
            })
            .transpose()?
            .unwrap_or(0);

        Ok(Some((n, sum_dl)))
    }

    #[doc(hidden)]
    pub fn fts_doc_len(
        &self,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.fts_doc_len_impl(&rtxn, label, property, node_id)
    }

    pub(super) fn fts_doc_len_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
        node_id: NodeId,
    ) -> Result<Option<u32>, Error> {
        let label_id = match get_label(&self.storage, rtxn, label)? {
            Some(id) => id,
            None => return Ok(None),
        };
        let prop_key_id = match get_prop_key(&self.storage, rtxn, property)? {
            Some(id) => id,
            None => return Ok(None),
        };

        let d_key = fts_doc_key(label_id, prop_key_id, node_id);
        if let Some(bytes) = self.storage.fts_docs.get(rtxn, &d_key)? {
            let doc_len = parse_fts_doc_val(bytes)?;
            Ok(Some(doc_len))
        } else {
            Ok(None)
        }
    }

    #[doc(hidden)]
    pub fn fts_postings(
        &self,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.fts_postings_impl(&rtxn, label, property, term)
    }

    pub(super) fn fts_postings_impl(
        &self,
        rtxn: &heed::RoTxn,
        label: &str,
        property: &str,
        term: &str,
    ) -> Result<Vec<(NodeId, u32)>, Error> {
        let label_id = match get_label(&self.storage, rtxn, label)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let prop_key_id = match get_prop_key(&self.storage, rtxn, property)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };

        let p_key = fts_postings_key(label_id, prop_key_id, term);
        let mut postings = Vec::new();
        if let Some(iter) = self.storage.fts_postings.get_duplicates(rtxn, &p_key)? {
            for result in iter {
                let (_, bytes) = result?;
                let (node_id, freq) = parse_fts_posting_val(bytes)?;
                postings.push((node_id, freq));
            }
        }

        Ok(postings)
    }
}
