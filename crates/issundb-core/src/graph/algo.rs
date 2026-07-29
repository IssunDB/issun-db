use super::*;

#[cfg(test)]
thread_local! {
    /// Test-only override for [`Graph::kernel_threads`], so a unit test can drive
    /// the parallel reduction on a graph small enough to build in a test.
    ///
    /// It is thread-local rather than process-global because the test binary runs its
    /// tests concurrently, and a global would let the forcing test change the worker
    /// count every other test sees for as long as it holds the override. That
    /// would put unrelated tests on the parallel path (spawning up to the forced
    /// count) and make coverage of the reduction nondeterministic.
    /// `kernel_threads` reads this on the calling thread, so a thread-local is
    /// read where it is set.
    pub(super) static FORCE_KERNEL_THREADS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

impl Graph {
    // ------------------------------------------------------------------
    // Graph algorithms
    // ------------------------------------------------------------------

    /// Depth-first search outward from `start` up to `hops` levels deep.
    pub fn dfs(&self, start: NodeId, hops: u8) -> Result<Vec<NodeId>, Error> {
        self.with_snapshot(|snap| self.dfs_kernel(snap, start, hops))
    }

    /// Counts variable assignments of the directed triangle pattern
    /// `(a)-[t1]->(b)-[t2]->(c)-[t3]->(a)` under `spec`'s per-hop relationship
    /// types and per-variable labels.
    ///
    /// The count follows Cypher MATCH semantics: each distinct assignment of
    /// `(a, b, c, e1, e2, e3)` is one match, so a single 3-cycle of distinct
    /// nodes counts once per rotation of `a` (three when all hops share one
    /// type), parallel edges multiply, and the three relationships must be
    /// pairwise distinct (relationship uniqueness), which only constrains
    /// self-loop assignments where `a == b == c`.
    pub fn count_triangle_cycles(&self, spec: &TriangleCountSpec) -> Result<u64, Error> {
        // Snapshot-only gate: this kernel reads CSR arrays and never a matrix,
        self.ensure_snapshot_fresh()?;
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(0);
        }

        // A named but unregistered relationship type matches nothing.
        let mut type_ids: [Option<TypeId>; 3] = [None; 3];
        {
            let rtxn = self.storage.env.read_txn()?;
            for (i, name) in spec.rel_types.iter().enumerate() {
                if let Some(name) = name {
                    match get_type(&self.storage, &rtxn, name)? {
                        Some(tid) => type_ids[i] = Some(tid),
                        None => return Ok(0),
                    }
                }
            }
        }

        // Dense-index masks for the per-variable labels; `None` means
        // unconstrained. An unknown label yields an all-false mask, which
        // counts zero without a special case.
        // A pattern almost always repeats a label across its variables
        // (`(:Person)->(:Person)->(:Person)`), and building one mask costs a full
        // label-index scan plus a dense lookup per node. Build each *distinct*
        // label once and copy it for the repeats: the copy is a memcpy over the
        // dense space, against an index scan of the whole label. Each variable
        // still owns its mask, because a pushed-down `vertex_allow` intersects
        // into it in place.
        let mut masks: [Option<Vec<bool>>; 3] = [None, None, None];
        let mut built: Vec<(&str, Vec<bool>)> = Vec::new();
        for (i, label) in spec.labels.iter().enumerate() {
            let Some(name) = label else { continue };
            let at = match built.iter().position(|(seen, _)| seen == name) {
                Some(at) => at,
                None => {
                    let mut mask = vec![false; n];
                    for id in self.nodes_by_label(name)? {
                        if let Some(&d) = snap.id_to_dense.get(&id) {
                            mask[d as usize] = true;
                        }
                    }
                    built.push((name, mask));
                    built.len() - 1
                }
            };
            masks[i] = Some(built[at].1.clone());
        }
        let label_ok = |mask: &Option<Vec<bool>>, d: usize| mask.as_ref().is_none_or(|m| m[d]);

        // Sorted typed adjacency for each hop: hop 1 and hop 2 read forward
        // rows, hop 3 reads the transpose (edges into `a`). Hop 2 reuses the
        // hop-1 view when the types coincide.
        let out1 = typed_out_sorted(&snap, type_ids[0]);
        let out2_built = if type_ids[1] == type_ids[0] {
            None
        } else {
            Some(typed_out_sorted(&snap, type_ids[1]))
        };
        let out2 = out2_built.as_ref().unwrap_or(&out1);
        let in3 = typed_in_sorted(&snap, type_ids[2]);

        let mut total: u64 = 0;
        for a in 0..n {
            if !label_ok(&masks[0], a) {
                continue;
            }
            let in3_row = in3.row(a);
            if in3_row.is_empty() {
                continue;
            }
            let out1_row = out1.row(a);

            let mut i = 0;
            while i < out1_row.len() {
                let b = out1_row[i].0 as usize;
                let run1_start = i;
                while i < out1_row.len() && out1_row[i].0 as usize == b {
                    i += 1;
                }
                if !label_ok(&masks[1], b) {
                    continue;
                }
                let m1 = (i - run1_start) as u64;
                let out2_row = out2.row(b);

                // Sorted merge of the hop-2 candidates from `b` against the
                // hop-3 sources into `a`; equal runs give parallel-edge
                // multiplicities.
                let (mut j, mut k) = (0, 0);
                let mut pair_count: u64 = 0;
                while j < out2_row.len() && k < in3_row.len() {
                    let c2 = out2_row[j].0;
                    let c3 = in3_row[k].0;
                    match c2.cmp(&c3) {
                        std::cmp::Ordering::Less => j += 1,
                        std::cmp::Ordering::Greater => k += 1,
                        std::cmp::Ordering::Equal => {
                            let c = c2 as usize;
                            let j0 = j;
                            while j < out2_row.len() && out2_row[j].0 as usize == c {
                                j += 1;
                            }
                            let k0 = k;
                            while k < in3_row.len() && in3_row[k].0 as usize == c {
                                k += 1;
                            }
                            if !label_ok(&masks[2], c) {
                                continue;
                            }
                            if a == b && c == a {
                                // Every hop is a self-loop at `a`, the one shape
                                // where two hops can bind the same relationship.
                                // Enumerate ordered triples of pairwise-distinct
                                // edge IDs explicitly; this term replaces the
                                // multiplicity product for this cell, so it is
                                // not scaled by `m1`.
                                for &(_, e1) in &out1_row[run1_start..run1_start + m1 as usize] {
                                    for &(_, e2) in &out2_row[j0..j] {
                                        if e2 == e1 {
                                            continue;
                                        }
                                        for &(_, e3) in &in3_row[k0..k] {
                                            if e3 != e1 && e3 != e2 {
                                                total += 1;
                                            }
                                        }
                                    }
                                }
                            } else {
                                pair_count += ((j - j0) * (k - k0)) as u64;
                            }
                        }
                    }
                }
                total += m1 * pair_count;
            }
        }
        Ok(total)
    }

    /// Counts variable assignments of an open directed path of one or two hops
    /// under `spec`'s per-hop relationship types and per-variable labels, with
    /// no materialization of the matched rows.
    ///
    /// The count follows Cypher MATCH semantics: each distinct assignment of
    /// the node and relationship variables is one match, nodes may repeat,
    /// parallel edges multiply, and for the two-hop pattern the two
    /// relationships must be distinct (relationship uniqueness). That
    /// uniqueness only removes assignments where a single edge could fill both
    /// hops, which requires a self-loop shared by both hops.
    ///
    /// The Cypher optimizer lowers a grouping-free `count` over a one-hop or
    /// two-hop directed expansion to this kernel via the `PathCount` physical operator.
    pub fn count_linear_paths(&self, spec: &PathCountSpec) -> Result<u64, Error> {
        let hops = spec.rel_types.len();
        debug_assert!(hops == 1 || hops == 2, "count_linear_paths: 1 or 2 hops");
        debug_assert_eq!(spec.labels.len(), hops + 1, "labels must be hops + 1");

        // A named but unregistered relationship type matches nothing. Resolved
        // before the freshness gate below, because it reads only the type
        // registry: an unregistered type counts zero without any rebuild.
        let mut type_ids: Vec<Option<TypeId>> = vec![None; hops];
        {
            let rtxn = self.storage.env.read_txn()?;
            for (i, name) in spec.rel_types.iter().enumerate() {
                if let Some(name) = name {
                    match get_type(&self.storage, &rtxn, name)? {
                        Some(tid) => type_ids[i] = Some(tid),
                        None => return Ok(0),
                    }
                }
            }
        }

        // Snapshot-only gate: this kernel reads CSR arrays and never a matrix,
        self.ensure_snapshot_fresh()?;
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(0);
        }

        // Dense-index masks for the per-variable labels; `None` is
        // unconstrained. An unknown label yields an all-false mask, counting
        // zero without a special case.
        // Every variable in a path pattern usually carries the same label
        // (`(:Person)->(:Person)->(:Person)`), and one mask costs a full
        // label-index scan plus a dense lookup per node. Build each *distinct*
        // label once and copy it for the repeats: the copy is a memcpy over the
        // dense space, against an index scan of the whole label. Each variable
        // keeps its own mask, because a pushed-down `vertex_allow` intersects
        // into it in place.
        let mut masks: Vec<Option<Vec<bool>>> = vec![None; hops + 1];
        let mut built: Vec<(&str, Vec<bool>)> = Vec::new();
        for (i, label) in spec.labels.iter().enumerate() {
            let Some(name) = label else { continue };
            let at = match built.iter().position(|(seen, _)| seen == name) {
                Some(at) => at,
                None => {
                    let mut mask = vec![false; n];
                    for id in self.nodes_by_label(name)? {
                        if let Some(&d) = snap.id_to_dense.get(&id) {
                            mask[d as usize] = true;
                        }
                    }
                    built.push((name, mask));
                    built.len() - 1
                }
            };
            masks[i] = Some(built[at].1.clone());
        }
        // Per-variable allow-sets from pushed-down property predicates. A
        // present set intersects with the label mask (a node passes only when it
        // is in both); a node id absent from the snapshot maps to no dense index
        // and is simply dropped, counting zero without a special case. An empty
        // `vertex_allow` (the default) leaves every mask as the label mask, so an
        // unfiltered path count is unchanged.
        for (i, allow) in spec.vertex_allow.iter().enumerate() {
            let Some(ids) = allow else { continue };
            let mut amask = vec![false; n];
            for &id in ids {
                if let Some(&d) = snap.id_to_dense.get(&id) {
                    amask[d as usize] = true;
                }
            }
            match &mut masks[i] {
                Some(m) => {
                    for (slot, &keep) in m.iter_mut().zip(amask.iter()) {
                        *slot = *slot && keep;
                    }
                }
                None => masks[i] = Some(amask),
            }
        }
        let label_ok = |mask: &Option<Vec<bool>>, d: usize| mask.as_ref().is_none_or(|m| m[d]);

        // Counting needs neighbor ids and row boundaries only, so both branches
        // below read the snapshot's own CSR arrays and filter by type inline.
        // Materializing a per-type sorted copy of the adjacency (as the triangle
        // kernel does, where sorted rows enable merge intersections) would
        // allocate and sort the whole edge set on every call for no benefit
        // here: the only consumer of that order was the self-loop lookup, which
        // is now a direct scan of the middle node's own row.
        let type_ok = |want: Option<TypeId>, have: TypeId| want.is_none_or(|t| have == t);

        if hops == 1 {
            // Count typed edges `v0 -> v1` with `v0` and `v1` inside their masks.
            let mut total: u64 = 0;
            for v0 in 0..n {
                if !label_ok(&masks[0], v0) {
                    continue;
                }
                for idx in snap.row_ptr[v0]..snap.row_ptr[v0 + 1] {
                    if type_ok(type_ids[0], snap.edge_type[idx])
                        && label_ok(&masks[1], snap.col_idx[idx] as usize)
                    {
                        total += 1;
                    }
                }
            }
            return Ok(total);
        }

        // Two hops `(v0:m0)-[t1]->(v1:m1)-[t2]->(v2:m2)`. The path count
        // factors through the middle node: for each `v1`, the number of
        // matches is the count of qualifying hop-1 in-edges times the count of
        // qualifying hop-2 out-edges. Relationship uniqueness then removes the
        // assignments where hop 1 and hop 2 bind the same edge, which is only
        // possible for a self-loop at `v1` that satisfies both hops.
        let (t1, t2) = (type_ids[0], type_ids[1]);
        // The per-middle-node contributions are independent, so the count is a
        // reduction over disjoint node ranges: each worker sums its own range of
        // `b` and the ranges are added at the end. Every array read is through the
        // immutable snapshot, so no worker synchronizes with any other.
        let snap_ref: &CsrSnapshot = &snap;
        let masks_ref = &masks;
        let count_middles = move |lo: usize, hi: usize| -> u64 {
            let type_ok = |want: Option<TypeId>, have: TypeId| want.is_none_or(|t| have == t);
            let label_ok = |mask: &Option<Vec<bool>>, d: usize| mask.as_ref().is_none_or(|m| m[d]);
            let (snap, masks) = (snap_ref, masks_ref);
            let mut total: u64 = 0;
            for b in lo..hi {
                if !label_ok(&masks[1], b) {
                    continue;
                }
                // Hop-1 in-edges of `b`: type `t1`, source inside the first mask.
                // The transposed view is part of the snapshot, so this is a scan of
                // one contiguous row.
                let mut indeg: u64 = 0;
                for idx in snap.in_row_ptr[b]..snap.in_row_ptr[b + 1] {
                    if type_ok(t1, snap.in_edge_type[idx])
                        && label_ok(&masks[0], snap.in_col_idx[idx] as usize)
                    {
                        indeg += 1;
                    }
                }
                if indeg == 0 {
                    continue;
                }
                // Hop-2 out-edges of `b`: type `t2`, destination inside the last mask.
                let mut outdeg: u64 = 0;
                for idx in snap.row_ptr[b]..snap.row_ptr[b + 1] {
                    if type_ok(t2, snap.edge_type[idx])
                        && label_ok(&masks[2], snap.col_idx[idx] as usize)
                    {
                        outdeg += 1;
                    }
                }
                total += indeg * outdeg;

                // Relationship-uniqueness correction. A single edge can fill both
                // hops only when it is a self-loop at `b` whose type satisfies both
                // hops, and `b` satisfies the first and last masks. Each such edge
                // is counted once in `indeg` and once in `outdeg`, so it contributes
                // exactly one `r1 == r2` assignment to the product: the number of
                // excluded assignments is the number of those self-loops, which
                // parallel self-loops make greater than one. Counting them by type
                // is equivalent to intersecting the two rows by edge id, because an
                // edge id identifies one edge and a self-loop at `b` appears once in
                // each row.
                if label_ok(&masks[0], b) && label_ok(&masks[2], b) {
                    let mut shared: u64 = 0;
                    for idx in snap.row_ptr[b]..snap.row_ptr[b + 1] {
                        if snap.col_idx[idx] as usize == b
                            && type_ok(t1, snap.edge_type[idx])
                            && type_ok(t2, snap.edge_type[idx])
                        {
                            shared += 1;
                        }
                    }
                    total = total.saturating_sub(shared);
                }
            }
            total
        };

        let threads = self.kernel_threads(n.saturating_add(snap.col_idx.len()));
        if threads <= 1 {
            return Ok(count_middles(0, n));
        }
        let chunk = n.div_ceil(threads);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|t| {
                    let lo = (t * chunk).min(n);
                    let hi = lo.saturating_add(chunk).min(n);
                    scope.spawn(move || count_middles(lo, hi))
                })
                .collect();
            let mut total: u64 = 0;
            for worker in workers {
                // A worker only reads the snapshot, so a panic here is a bug, not
                // a data condition; surface it instead of returning a short count.
                match worker.join() {
                    Ok(part) => total = total.saturating_add(part),
                    Err(_) => return Err(Error::Corrupt("path-count worker panicked")),
                }
            }
            Ok(total)
        })
    }

    /// Counts typed edges grouped by one endpoint, returning `(group node id, count)`
    /// for every group node with a non-zero count. See [`GroupedDegreeSpec`]
    /// for the grouping and filtering semantics.
    ///
    /// This scans the CSR snapshot's outgoing adjacency once, incrementing a
    /// per-node counter, so it is `O(nodes + edges)` with no per-edge row
    /// materialization. It is the kernel the Cypher optimizer lowers a
    /// `count` aggregation grouped by one endpoint of a single directed hop
    /// to (the `GroupedDegree` physical operator), turning what would be a
    /// full expansion-and-fold into an integer pass over adjacency.
    pub fn grouped_edge_counts(
        &self,
        spec: &GroupedDegreeSpec,
    ) -> Result<Vec<(NodeId, u64)>, Error> {
        // Snapshot-only gate: this kernel reads CSR arrays and never a matrix,
        self.ensure_snapshot_fresh()?;
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        if n == 0 {
            return Ok(Vec::new());
        }

        // A named but unregistered relationship type matches nothing.
        let type_id = match spec.rel_type {
            Some(name) => {
                let rtxn = self.storage.env.read_txn()?;
                match get_type(&self.storage, &rtxn, name)? {
                    Some(tid) => Some(tid),
                    None => return Ok(Vec::new()),
                }
            }
            None => None,
        };

        // Dense label masks; an unknown label yields an all-false mask, which
        // counts zero without a special case.
        let label_mask = |label: Option<&str>| -> Result<Option<Vec<bool>>, Error> {
            match label {
                Some(name) => {
                    let mut mask = vec![false; n];
                    for id in self.nodes_by_label(name)? {
                        if let Some(&d) = snap.id_to_dense.get(&id) {
                            mask[d as usize] = true;
                        }
                    }
                    Ok(Some(mask))
                }
                None => Ok(None),
            }
        };
        let group_mask = label_mask(spec.group_label)?;
        // The endpoints usually carry the same label (e.g. `(:Person)->(:Person)`);
        // reuse the mask instead of scanning that label a second time.
        let mut counted_mask = if spec.counted_label == spec.group_label {
            group_mask.clone()
        } else {
            label_mask(spec.counted_label)?
        };
        // An explicit allow-set narrows the counted endpoint further; an empty one
        // yields an all-false mask, which counts zero without a special case.
        if let Some(allow) = spec.counted_allow {
            let mut allowed = vec![false; n];
            for id in allow {
                if let Some(&d) = snap.id_to_dense.get(id) {
                    allowed[d as usize] = true;
                }
            }
            counted_mask = Some(match counted_mask {
                None => allowed,
                Some(prev) => {
                    let mut both = allowed;
                    for (slot, &keep) in both.iter_mut().zip(prev.iter()) {
                        *slot = *slot && keep;
                    }
                    both
                }
            });
        }

        // One pass over the qualifying edges, shared by both walks below so their
        // type and label filters cannot drift apart.
        fn walk_qualifying<F: FnMut(usize, usize)>(
            snap: &CsrSnapshot,
            n: usize,
            type_id: Option<TypeId>,
            group_is_dst: bool,
            group_mask: &Option<Vec<bool>>,
            counted_mask: &Option<Vec<bool>>,
            mut visit: F,
        ) {
            let ok = |mask: &Option<Vec<bool>>, d: usize| mask.as_ref().is_none_or(|m| m[d]);
            for v0 in 0..n {
                for k in snap.row_ptr[v0]..snap.row_ptr[v0 + 1] {
                    if let Some(tid) = type_id {
                        if snap.edge_type[k] != tid {
                            continue;
                        }
                    }
                    let v1 = snap.col_idx[k] as usize;
                    // Map the stored edge `v0 -> v1` to the group and counted
                    // endpoints per the grouping direction.
                    let (group_d, counted_d) = if group_is_dst { (v1, v0) } else { (v0, v1) };
                    // Label constraints decide which edges match (existence); the
                    // non-null property filter only narrows the count within them.
                    if !ok(group_mask, group_d) || !ok(counted_mask, counted_d) {
                        continue;
                    }
                    visit(group_d, counted_d);
                }
            }
        }

        // `present` marks a group node with at least one label-qualifying edge,
        // so it produces a MATCH row and therefore a group. `qualifying` counts
        // those edges. For `count(*)` that is already the answer; for
        // `count(v.prop)` the tally narrows to the edges whose counted endpoint is
        // non-null, and the two differ: a group can exist (an edge reaches it)
        // while its count is zero (every counted source has a null property), and
        // that group must still appear with count zero, exactly as the row
        // pipeline emits it.
        let mut qualifying = vec![0u64; n];
        let mut present = vec![false; n];
        // Only the non-null filter needs to know which counted endpoints were
        // reached, so `count(*)`, the common shape, allocates no bitmap and pays no
        // store per traversed edge.
        let mut visited_counted = if spec.counted_nonnull_prop.is_some() {
            vec![false; n]
        } else {
            Vec::new()
        };
        if spec.counted_nonnull_prop.is_none() {
            walk_qualifying(
                &snap,
                n,
                type_id,
                spec.group_is_dst,
                &group_mask,
                &counted_mask,
                |group_d, _| {
                    present[group_d] = true;
                    qualifying[group_d] += 1;
                },
            );
        } else {
            walk_qualifying(
                &snap,
                n,
                type_id,
                spec.group_is_dst,
                &group_mask,
                &counted_mask,
                |group_d, counted_d| {
                    present[group_d] = true;
                    qualifying[group_d] += 1;
                    visited_counted[counted_d] = true;
                },
            );
        }

        // Resolve the non-null filter over the endpoints the walk actually reached,
        // and re-tally only when some of them really are null. The second pass is
        // the price of resolving presence for the visited set rather than trusting a
        // whole-column summary, which was unsound; it is paid only for
        // `count(prop)` and only when a null is actually present.
        let counts = match spec.counted_nonnull_prop {
            None => qualifying,
            Some(prop) => match self.visited_nonnull_mask(&snap, &visited_counted, prop)? {
                None => qualifying,
                Some(mask) => {
                    let mut counts = vec![0u64; n];
                    walk_qualifying(
                        &snap,
                        n,
                        type_id,
                        spec.group_is_dst,
                        &group_mask,
                        &counted_mask,
                        |group_d, counted_d| {
                            if mask[counted_d] {
                                counts[group_d] += 1;
                            }
                        },
                    );
                    counts
                }
            },
        };

        let mut out = Vec::new();
        for (d, &p) in present.iter().enumerate() {
            if p {
                out.push((snap.dense_to_id[d], counts[d]));
            }
        }
        Ok(out)
    }

    /// Dense non-null mask over the snapshot for `prop`, resolved for exactly the
    /// nodes `visited` marks.
    ///
    /// `None` means every visited node carries a non-null value, so the caller can
    /// skip the mask entirely. That is sound only because the caller queries the
    /// mask for visited nodes and no others, which is the same set this resolved:
    /// a kernel tests the non-null filter only on an endpoint that already passed
    /// its label and allow-set filters, and those are exactly the endpoints it
    /// marked.
    ///
    /// Resolving the visited nodes rather than the whole graph is what keeps a
    /// `count(v.prop)` collapse off a full node scan. The presence read goes
    /// through the same small-request path a point read uses, so a pass over a
    /// handful of neighbors costs a handful of point reads instead of building
    /// every property column, which on a large graph is the dominant cost and the
    /// one a lazily opened graph exists to defer.
    ///
    /// It is also why there is no "this column has no nulls anywhere, skip the
    /// mask" shortcut. That test needed the column set and the snapshot to cover
    /// the same nodes, and it compared their *counts*: equal counts do not imply
    /// equal sets, so a deletion and an insertion landing between the snapshot
    /// refresh and the column refresh would pass the size test while the snapshot
    /// still held a node the columns never saw, and that node's edges would count
    /// as non-null. Resolving per visited node needs no such coverage assumption.
    fn visited_nonnull_mask(
        &self,
        snap: &CsrSnapshot,
        visited: &[bool],
        prop: &str,
    ) -> Result<Option<Vec<bool>>, Error> {
        let ids: Vec<NodeId> = visited
            .iter()
            .enumerate()
            .filter(|&(_, &seen)| seen)
            .map(|(d, _)| snap.dense_to_id[d])
            .collect();
        if ids.is_empty() {
            return Ok(None);
        }
        let present = self.nodes_prop_present(&ids, prop)?;
        if present.iter().all(|&p| p) {
            return Ok(None);
        }
        let mut mask = vec![false; visited.len()];
        for (id, is_present) in ids.iter().zip(present) {
            if is_present {
                if let Some(&d) = snap.id_to_dense.get(id) {
                    mask[d as usize] = true;
                }
            }
        }
        Ok(Some(mask))
    }

    /// Threads to spread a read-only kernel pass over, given the number of items
    /// it will touch.
    ///
    /// A counting kernel is a pure reduction over disjoint slices of the CSR
    /// arrays, so it parallelizes without synchronization; the arrays are read
    /// through an immutable snapshot, so this takes no lock and never races a
    /// writer. The count itself comes from [`crate::threads::resolve`], the single
    /// resolution every parallel consumer shares, so the one knob means the same
    /// thing here as it does for the analytics passes.
    ///
    /// A small pass stays single-threaded: below the threshold the spawn cost
    /// exceeds the saving, which also keeps unit tests deterministic and off the
    /// thread pool entirely.
    pub(super) fn kernel_threads(&self, work: usize) -> usize {
        // A counting pass streams adjacency arrays, so it saturates memory
        // bandwidth long before it saturates compute, and past its peak extra
        // workers add traffic and coordination without adding throughput. Measured
        // on the two-hop path count over an 11.1 M-edge graph (12-thread machine):
        // 71.1 ms at one worker, 47.0 at two, 40.5 at four, 43.8 at eight, 46.1 at
        // twelve. Twelve is 14% *slower* than four, so the resolved budget must not
        // be spent in full here: cap the split at the peak.
        //
        // The cap is calibrated on one machine. Re-measure the curve on hardware
        // with a different memory subsystem before treating four as general; the
        // test override bypasses it so a test can still drive more workers.
        const MAX_SCAN_THREADS: usize = 4;
        self.parallel_threads(work).min(MAX_SCAN_THREADS)
    }

    /// Threads for a pass whose cost is arithmetic per source rather than a stream
    /// over the adjacency arrays.
    ///
    /// This is [`Graph::kernel_threads`] without the memory-bandwidth cap, and the
    /// difference is the point: an all-pairs pass (betweenness, harmonic centrality)
    /// does `O(n * m)` work from per-worker buffers with no shared writes, so it keeps
    /// scaling past the four workers a streaming reduction peaks at. Capping it too
    /// would silently ignore most of the budget `set_thread_count` was given.
    pub(super) fn parallel_threads(&self, work: usize) -> usize {
        /// Items below which a pass is not worth splitting.
        const MIN_PARALLEL_WORK: usize = 1 << 18;
        // Tests force the split on graphs far below the threshold, so the
        // parallel reduction is exercised rather than only its fallback.
        #[cfg(test)]
        {
            let forced = FORCE_KERNEL_THREADS.with(|f| f.get());
            if forced > 0 {
                return forced;
            }
        }
        if work < MIN_PARALLEL_WORK {
            return 1;
        }
        crate::threads::resolve(self.n_threads.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Whether expanding `sources` from storage would beat bringing the CSR
    /// snapshot up to date.
    ///
    /// The counting kernels read the snapshot, so they must gate on
    /// `ensure_snapshot_fresh`, which is an `O(nodes + edges)` rebuild when a write
    /// has landed since the last build. Bulk expansion has always had an escape
    /// hatch for that case: a handful of sources over a stale snapshot is served
    /// from per-source adjacency with no rebuild. A caller choosing between a
    /// kernel and a per-source path should consult this first, so an interleaved
    /// write-then-count session does not pay a full rebuild per query.
    pub fn prefers_point_expansion(&self, sources: usize) -> bool {
        self.csr_cache.snapshot_is_stale()
            && sources <= crate::graph::kernels::traversal::STALE_POINT_EXPAND_MAX
    }

    /// Total length of `sources`' adjacency rows in the given direction: an upper
    /// bound on the edges [`Graph::typed_neighbor_counts`] would visit for them,
    /// before any type or label narrowing.
    ///
    /// This reads two array elements per source and no edge at all, so a caller
    /// can size an expansion before choosing how to evaluate it. A source absent
    /// from the snapshot contributes zero.
    pub fn adjacency_span(&self, sources: &[NodeId], incoming: bool) -> Result<u64, Error> {
        // Deliberately no freshness gate. This measures the installed snapshot so a
        // caller can decide whether an expansion is worth doing, and callers use it
        // precisely to avoid provoking a rebuild; refreshing here made the sizing
        // call perform the very work it exists to help skip. The answer is advisory,
        // so a stale or absent snapshot giving a low span is sound: the caller either
        // proceeds (and its own gate refreshes) or declines to a path that needs no
        // snapshot at all. A source the snapshot does not know contributes zero.
        let snap = self.csr_cache.snapshot.load();
        let row_ptr = if incoming {
            &snap.in_row_ptr
        } else {
            &snap.row_ptr
        };
        let mut span = 0u64;
        for src in sources {
            if let Some(&d) = snap.id_to_dense.get(src) {
                let d = d as usize;
                span = span.saturating_add((row_ptr[d + 1] - row_ptr[d]) as u64);
            }
        }
        Ok(span)
    }

    /// Counts each source's qualifying neighbors across one typed hop, returning
    /// `(qualifying, counted)` per entry of `sources` in input order. See
    /// [`NeighborCountSpec`] for what qualifies; the two totals differ only for
    /// `neighbor_nonnull_prop`, where a neighbor can qualify (so the source
    /// produces rows) without adding to the count.
    ///
    /// This reads only the sources' own CSR rows, so it costs the sum of their
    /// degrees rather than a full scan, and it tallies into integers without
    /// materializing one entry per traversed edge. It is the kernel behind the
    /// Cypher executor's terminal count-collapse, where the alternative is a
    /// bulk expansion whose result is one triple per edge plus a hash lookup per
    /// edge to qualify and tally it. Parallel edges each count, and a self-loop
    /// counts its source once, matching a materialized expansion row for row.
    ///
    /// A source absent from the snapshot has no neighbors and counts zero, so a
    /// caller need not pre-filter the source list.
    pub fn typed_neighbor_counts(
        &self,
        sources: &[NodeId],
        spec: &NeighborCountSpec,
    ) -> Result<Vec<(u64, u64)>, Error> {
        // Snapshot-only gate: this kernel reads CSR arrays and never a matrix,
        self.ensure_snapshot_fresh()?;
        let snap = self.csr_cache.snapshot.load();
        let n = snap.dense_to_id.len();
        let mut out = vec![(0u64, 0u64); sources.len()];
        if n == 0 || sources.is_empty() {
            return Ok(out);
        }

        // A named but unregistered relationship type matches nothing.
        let type_id = match spec.rel_type {
            Some(name) => {
                let rtxn = self.storage.env.read_txn()?;
                match get_type(&self.storage, &rtxn, name)? {
                    Some(tid) => Some(tid),
                    None => return Ok(out),
                }
            }
            None => None,
        };

        // Dense conjunction of the neighbor labels and the explicit allow-set; an
        // unknown label or an empty allow-set yields an all-false mask, which
        // counts zero without a special case.
        let mut label_mask: Option<Vec<bool>> = None;
        let intersect = |acc: &mut Option<Vec<bool>>, mask: Vec<bool>| match acc {
            None => *acc = Some(mask),
            Some(prev) => {
                for (slot, keep) in prev.iter_mut().zip(mask) {
                    *slot = *slot && keep;
                }
            }
        };
        for name in spec.neighbor_labels {
            let mut mask = vec![false; n];
            for id in self.nodes_by_label(name)? {
                if let Some(&d) = snap.id_to_dense.get(&id) {
                    mask[d as usize] = true;
                }
            }
            intersect(&mut label_mask, mask);
        }
        if let Some(allow) = spec.neighbor_allow {
            let mut mask = vec![false; n];
            for id in allow {
                if let Some(&d) = snap.id_to_dense.get(id) {
                    mask[d as usize] = true;
                }
            }
            intersect(&mut label_mask, mask);
        }

        let (row_ptr, col_idx, edge_type) = if spec.incoming {
            (&snap.in_row_ptr, &snap.in_col_idx, &snap.in_edge_type)
        } else {
            (&snap.row_ptr, &snap.col_idx, &snap.edge_type)
        };

        // One pass over one source's qualifying neighbors. A nested `fn` generic
        // over the visitor rather than a closure taking `&mut dyn FnMut`, so the
        // visitor inlines: this runs once per traversed edge, and an indirect call
        // there would be a cost the whole kernel exists to avoid. Shared by both
        // walks below so their type and label filters cannot drift apart.
        fn walk_source<F: FnMut(usize)>(
            d: usize,
            row_ptr: &[usize],
            col_idx: &[u32],
            edge_type: &[TypeId],
            type_id: Option<TypeId>,
            label_mask: &Option<Vec<bool>>,
            mut visit: F,
        ) {
            for k in row_ptr[d]..row_ptr[d + 1] {
                if let Some(tid) = type_id {
                    if edge_type[k] != tid {
                        continue;
                    }
                }
                let other = col_idx[k] as usize;
                if label_mask.as_ref().is_none_or(|m| m[other]) {
                    visit(other);
                }
            }
        }

        // The non-null filter is what needs to know which neighbors were reached,
        // so the bitmap is allocated only for that case. `count(*)` is the common
        // shape and must not pay for a whole-graph vector it never reads, nor a
        // store per traversed edge.
        let mut visited = if spec.neighbor_nonnull_prop.is_some() {
            vec![false; n]
        } else {
            Vec::new()
        };

        // First pass: the qualifying tally, which is already the answer for
        // `count(*)`.
        for (i, src) in sources.iter().enumerate() {
            let Some(&d) = snap.id_to_dense.get(src) else {
                continue;
            };
            let mut qualifying = 0u64;
            if spec.neighbor_nonnull_prop.is_none() {
                walk_source(
                    d as usize,
                    row_ptr,
                    col_idx,
                    edge_type,
                    type_id,
                    &label_mask,
                    |_| qualifying += 1,
                );
            } else {
                walk_source(
                    d as usize,
                    row_ptr,
                    col_idx,
                    edge_type,
                    type_id,
                    &label_mask,
                    |other| {
                        qualifying += 1;
                        visited[other] = true;
                    },
                );
            }
            out[i] = (qualifying, qualifying);
        }

        // Resolve the non-null filter over the neighbors the walk actually
        // reached, and re-tally only when some of them really are null.
        if let Some(prop) = spec.neighbor_nonnull_prop {
            if let Some(mask) = self.visited_nonnull_mask(&snap, &visited, prop)? {
                for (i, src) in sources.iter().enumerate() {
                    let Some(&d) = snap.id_to_dense.get(src) else {
                        continue;
                    };
                    let mut counted = 0u64;
                    walk_source(
                        d as usize,
                        row_ptr,
                        col_idx,
                        edge_type,
                        type_id,
                        &label_mask,
                        |other| {
                            if mask[other] {
                                counted += 1;
                            }
                        },
                    );
                    out[i].1 = counted;
                }
            }
        }
        Ok(out)
    }

    /// Detects if there is at least one directed cycle in the graph.
    pub fn detect_cycle(&self) -> Result<bool, Error> {
        self.with_snapshot(|snap| self.detect_cycle_kernel(snap))
    }

    /// Returns directed neighbor entries for all outgoing and incoming edges of `node`.
    pub fn all_neighbors(&self, node: NodeId) -> Result<Vec<DirectedNeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.all_neighbors_impl(&rtxn, node)
    }

    /// `all_neighbors` against a caller-supplied transaction, shared with the
    /// `WriteTxn` delegation so a write transaction's view sees its own
    /// uncommitted edges.
    pub(super) fn all_neighbors_impl(
        &self,
        txn: &heed::RoTxn,
        node: NodeId,
    ) -> Result<Vec<DirectedNeighborEntry>, Error> {
        let mut neighbors = Vec::new();
        for ne in self.out_neighbors_impl(txn, node)? {
            neighbors.push(DirectedNeighborEntry {
                node: ne.node,
                edge: ne.edge,
                edge_type: ne.edge_type,
                outgoing: true,
            });
        }
        for ne in self.in_neighbors_impl(txn, node)? {
            neighbors.push(DirectedNeighborEntry {
                node: ne.node,
                edge: ne.edge,
                edge_type: ne.edge_type,
                outgoing: false,
            });
        }
        Ok(neighbors)
    }

    /// Returns all simple paths (no repeated nodes) between `src` and `dst`.
    pub fn all_paths(&self, src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error> {
        self.with_snapshot(|snap| self.all_paths_kernel(snap, src, dst))
    }

    /// Returns all unweighted shortest paths between `src` and `dst`.
    pub fn all_shortest_paths(&self, src: NodeId, dst: NodeId) -> Result<Vec<Vec<NodeId>>, Error> {
        self.with_snapshot(|snap| self.all_shortest_paths_kernel(snap, src, dst))
    }

    /// Returns the longest simple path (no repeated nodes) between `src` and `dst`.
    pub fn longest_path(&self, src: NodeId, dst: NodeId) -> Result<Option<Vec<NodeId>>, Error> {
        self.with_snapshot(|snap| self.longest_path_kernel(snap, src, dst))
    }

    /// Computes the weighted shortest path between `src` and `dst` using Dijkstra's algorithm.
    ///
    /// Edge weights come from the CSR snapshot, which reads the first present of
    /// the `weight`, `cost`, `capacity`, or `cap` edge properties, defaulting to
    /// `1.0`. The weight source is fixed: unlike `shortest_path_top_k` and
    /// `spanning_forest`, this method does not take a weight-property argument.
    ///
    /// This is the one algorithm needing a *weighted* snapshot, which costs a second
    /// scan of `edges` to load; see [`Graph::with_weighted_snapshot`].
    pub fn shortest_path_dijkstra(
        &self,
        src: NodeId,
        dst: NodeId,
    ) -> Result<Option<WeightedPath>, Error> {
        self.with_weighted_snapshot(|snap| self.shortest_path_dijkstra_kernel(snap, src, dst))
    }

    /// Computes the Minimum or Maximum Spanning Forest (MSF) of the graph.
    pub fn spanning_forest(
        &self,
        weight_property: &str,
        maximum: bool,
    ) -> Result<Vec<EdgeId>, Error> {
        self.with_snapshot(|snap| self.spanning_forest_kernel(snap, weight_property, maximum))
    }

    /// Computes community detection on the graph using the Label Propagation Algorithm (LPA / CDLP).
    pub fn label_propagation(&self, max_iterations: usize) -> Result<HashMap<NodeId, u64>, Error> {
        self.label_propagation_kernel(max_iterations)
    }

    /// Computes the harmonic closeness centrality for all nodes in the graph.
    pub fn harmonic_centrality(&self) -> Result<HashMap<NodeId, f64>, Error> {
        self.with_snapshot(|snap| self.harmonic_centrality_kernel(snap))
    }

    /// Computes the betweenness centrality for all nodes in the graph.
    pub fn betweenness_centrality(&self) -> Result<HashMap<NodeId, f64>, Error> {
        self.with_snapshot(|snap| self.betweenness_centrality_kernel(snap))
    }

    /// Computes the strongly connected components (SCC) of the graph using Tarjan's algorithm.
    pub fn strongly_connected_components(&self) -> Result<HashMap<NodeId, u64>, Error> {
        self.with_snapshot(|snap| self.strongly_connected_components_kernel(snap))
    }

    /// Computes the degree centrality for all nodes in the graph based on the
    /// specified direction. Counts *distinct* neighbors; see
    /// [`Graph::degree_centrality_kernel`].
    pub fn degree_centrality(
        &self,
        direction: DegreeDirection,
    ) -> Result<HashMap<NodeId, u64>, Error> {
        self.with_snapshot(|snap| self.degree_centrality_kernel(snap, direction))
    }

    /// Computes the maximum flow from a source node to a sink node.
    pub fn maximum_flow(
        &self,
        source: NodeId,
        sink: NodeId,
        capacity_property: &str,
    ) -> Result<f64, Error> {
        self.with_snapshot(|snap| self.maximum_flow_kernel(snap, source, sink, capacity_property))
    }

    /// Computes the K shortest paths from a source node to a destination node using Yen's algorithm.
    pub fn shortest_path_top_k(
        &self,
        src: NodeId,
        dst: NodeId,
        k: usize,
        weight_property: &str,
    ) -> Result<Vec<WeightedPath>, Error> {
        let paths = self.with_snapshot(|snap| {
            self.shortest_path_top_k_kernel(snap, src, dst, k, weight_property)
        })?;
        Ok(paths
            .into_iter()
            .map(|(nodes, total_weight)| WeightedPath {
                nodes,
                total_weight,
            })
            .collect())
    }

    /// Run `f` over a CSR snapshot that reflects every committed write.
    ///
    /// This is the one gate in front of every algorithm kernel. All of them read the
    /// snapshot and nothing else, so there is a single freshness condition to
    /// satisfy: the installed snapshot's generation against the committed-write
    /// generation. Point adjacency lookups (`out_neighbors` and friends) do not come
    /// through here at all; they read LMDB directly and so always see the latest
    /// write, in or out of a transaction.
    ///
    /// The snapshot arrives as an `ArcSwap` load, so `f` reads one immutable
    /// snapshot throughout even if a concurrent refresh installs a newer one
    /// meanwhile.
    pub(in crate::graph) fn with_snapshot<T>(
        &self,
        f: impl FnOnce(&CsrSnapshot) -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.ensure_snapshot_fresh()?;
        f(&self.csr_cache.snapshot.load())
    }

    /// [`Graph::with_snapshot`] for the one consumer that reads per-edge weights,
    /// so the snapshot it receives is guaranteed to carry them.
    pub(in crate::graph) fn with_weighted_snapshot<T>(
        &self,
        f: impl FnOnce(&CsrSnapshot) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let snap = self.weighted_snapshot()?;
        f(&snap)
    }

    /// A fresh snapshot carrying per-edge weights.
    ///
    /// Returns the snapshot it validated rather than leaving the caller to reload
    /// it. A concurrent unweighted refresh can replace the pointer between a gate
    /// and a load, and the caller would then find no weights on a snapshot the gate
    /// had just vouched for.
    ///
    /// Asking for weights is sticky, so every later rebuild keeps loading them.
    /// Without that, one unweighted refresh would strip them and the next weighted
    /// query would rebuild from storage again, so a workload alternating a weighted
    /// algorithm with any other would rebuild twice per write instead of once. The
    /// cost is eight bytes per edge held for as long as the snapshot lives, once
    /// anything asks a weighted question. That is the trade the weighted adjacency
    /// matrix made before it, at under half the size, and it is why `rebuild_csr`
    /// does not ask: bulk loads call that, and asking there would pin the weights in
    /// every process that ever loads data.
    fn weighted_snapshot(&self) -> Result<Arc<CsrSnapshot>, Error> {
        self.csr_cache.request_weights();
        let installed = self.csr_cache.snapshot.load_full();
        if !self.csr_cache.snapshot_is_stale() && installed.edge_weight.is_some() {
            return Ok(installed);
        }
        let _maint = self.csr_cache.maintenance.lock();
        // Re-check under the lock: another maintenance pass may have refreshed while
        // this thread waited.
        let installed = self.csr_cache.snapshot.load_full();
        if !self.csr_cache.snapshot_is_stale() && installed.edge_weight.is_some() {
            return Ok(installed);
        }
        let built_gen = self.csr_cache.current_gen();
        let snap = Arc::new(CsrSnapshot::build_weighted(&self.storage)?);
        self.csr_cache
            .install_snapshot_shared(Arc::clone(&snap), built_gen);
        Ok(snap)
    }

    /// Refresh the CSR snapshot when it lags committed writes.
    ///
    /// The generation counter is published immediately after the commit (see
    /// [`crate::csr::CsrCache::advance_write_gen`]), so what remains uncovered is
    /// the gap between LMDB making a write visible and that one increment, not the
    /// width of the write's bookkeeping. Closing it outright is a read-isolation
    /// question, not a gate question: a reader here holds no transaction, so it
    /// has no point in time to be consistent with in the first place.
    pub(crate) fn ensure_snapshot_fresh(&self) -> Result<(), Error> {
        // Lock-free pre-check.
        if !self.csr_cache.snapshot_is_stale() {
            return Ok(());
        }
        let _maint = self.csr_cache.maintenance.lock();
        // Re-check under the lock in case another pass already refreshed.
        if self.csr_cache.snapshot_is_stale() {
            let built_gen = self.csr_cache.current_gen();
            let snap = self.build_snapshot()?;
            self.csr_cache.install_snapshot(snap, built_gen);
        }
        Ok(())
    }

    /// Build a snapshot from storage, carrying weights when some consumer has asked
    /// for them. See [`Graph::weighted_snapshot`] for why that request is sticky.
    pub(super) fn build_snapshot(&self) -> Result<CsrSnapshot, Error> {
        if self.csr_cache.wants_weights() {
            CsrSnapshot::build_weighted(&self.storage)
        } else {
            CsrSnapshot::build(&self.storage)
        }
    }

    /// Returns all node IDs in the graph in ascending order.
    pub fn all_nodes(&self) -> Result<Vec<NodeId>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.all_nodes_impl(&rtxn)
    }

    pub(super) fn all_nodes_impl(&self, rtxn: &heed::RoTxn) -> Result<Vec<NodeId>, Error> {
        let mut ids = self
            .storage
            .nodes
            .iter(rtxn)?
            .map(|r| r.map(|(k, _)| k))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort_unstable();
        Ok(ids)
    }

    /// Weakly connected components, treating every edge as undirected.
    ///
    /// Returns a map from each node ID to a component ID. Only the induced
    /// partition is part of the contract; as it happens the id is the smallest node
    /// id in the component (see [`Graph::connected_components_kernel`]), but a caller
    /// should compare membership rather than depend on the numbering.
    pub fn connected_components(&self) -> Result<HashMap<NodeId, u64>, Error> {
        self.with_snapshot(|snap| self.connected_components_kernel(snap))
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Increment the dirty counter and, if the threshold is crossed and no
    /// rebuild is already running, spawn a background thread to rebuild the
    /// CSR snapshot from LMDB.
    ///
    /// This is the compaction safety net only. Marking the caches stale is a
    /// separate step that happens at commit time, in
    /// [`Graph::commit_and_publish`], so it cannot be delayed behind the rest of
    /// the post-commit bookkeeping.
    pub(super) fn maybe_spawn_rebuild(&self) {
        self.maybe_spawn_rebuild_n(1);
    }

    pub(super) fn maybe_spawn_rebuild_n(&self, count: usize) {
        if self.csr_cache.note_dirty_n(count as u64) {
            let cache = Arc::clone(&self.csr_cache);
            let storage = Arc::clone(&self.storage);
            std::thread::spawn(move || {
                // Rebuild until the dirty count drops below the threshold: writes
                // that commit while a rebuild runs keep the count above zero, and
                // `install` retains the claim and asks for another pass so the
                // snapshot does not silently lag behind LMDB.
                loop {
                    // Hold the maintenance lock across the whole pass (build plus
                    // install), reacquiring it each iteration so a foreground
                    // maintenance pass can interleave between passes. This serializes
                    // against any other rebuild, so two passes cannot install over
                    // each other.
                    let _maint = cache.maintenance.lock();
                    // Capture the generation before reading LMDB; writes that
                    // commit during the build leave the snapshot stale until the
                    // next pass, which the dirty-count loop already drives.
                    let built_gen = cache.current_gen();
                    let built = if cache.wants_weights() {
                        CsrSnapshot::build_weighted(&storage)
                    } else {
                        CsrSnapshot::build(&storage)
                    };
                    match built {
                        Ok(snap) => {
                            let again = cache.install(snap, built_gen);
                            if !again {
                                break;
                            }
                        }
                        Err(_) => {
                            // Nothing was installed, so `snapshot_gen` did not advance
                            // and the snapshot still reports itself stale: the next
                            // gated read rebuilds in the foreground and surfaces the
                            // error to its caller. Releasing the claim without
                            // clearing the dirty count is deliberate, so a later
                            // commit retries this pass rather than leaving compaction
                            // permanently disowned.
                            cache.cancel_rebuild();
                            break;
                        }
                    }
                }
            });
        }
    }

    /// Append one `AdjEntry` as a new LMDB duplicate value: O(log n), no blob read.
    pub(super) fn append_adj(
        &self,
        wtxn: &mut heed::RwTxn,
        node: NodeId,
        other: NodeId,
        edge_type: u32,
        edge_id: EdgeId,
        outgoing: bool,
    ) -> Result<(), Error> {
        let entry = AdjEntry {
            edge_type,
            other,
            edge_id,
        };
        let db = if outgoing {
            &self.storage.out_adj
        } else {
            &self.storage.in_adj
        };
        db.put(wtxn, &node, entry.as_bytes())?;
        Ok(())
    }

    /// Iterate all duplicate `AdjEntry` values for `node` via LMDB cursor.
    pub(super) fn adj_entries(
        &self,
        node: NodeId,
        outgoing: bool,
    ) -> Result<Vec<NeighborEntry>, Error> {
        let rtxn = self.storage.env.read_txn()?;
        self.adj_entries_impl(&rtxn, node, outgoing)
    }

    pub(super) fn adj_entries_impl(
        &self,
        rtxn: &heed::RoTxn,
        node: NodeId,
        outgoing: bool,
    ) -> Result<Vec<NeighborEntry>, Error> {
        let db = if outgoing {
            &self.storage.out_adj
        } else {
            &self.storage.in_adj
        };

        let iter = match db.get_duplicates(rtxn, &node)? {
            Some(iter) => iter,
            None => return Ok(vec![]),
        };

        let mut out = Vec::new();
        for result in iter {
            let (_, bytes) = result?;
            let entry = AdjEntry::decode_value(bytes)?;
            out.push(NeighborEntry {
                node: entry.other,
                edge: entry.edge_id,
                edge_type: entry.edge_type,
            });
        }
        Ok(out)
    }
}

/// Per-row adjacency restricted to one relationship type, with each row
/// sorted by `(neighbor, edge id)` so intersections run as sorted merges and
/// parallel edges form contiguous runs.
struct TypedSortedAdj {
    ptr: Vec<usize>,
    adj: Vec<(u32, EdgeId)>,
}

impl TypedSortedAdj {
    fn row(&self, d: usize) -> &[(u32, EdgeId)] {
        &self.adj[self.ptr[d]..self.ptr[d + 1]]
    }
}

/// Forward adjacency from the CSR snapshot filtered to `type_id` (`None`
/// keeps every edge), rows sorted by `(dst, edge id)`.
fn typed_out_sorted(snap: &CsrSnapshot, type_id: Option<TypeId>) -> TypedSortedAdj {
    let n = snap.dense_to_id.len();
    let keep = |idx: usize| type_id.is_none_or(|t| snap.edge_type[idx] == t);

    let mut ptr = vec![0usize; n + 1];
    for row in 0..n {
        let mut count = 0;
        for idx in snap.row_ptr[row]..snap.row_ptr[row + 1] {
            if keep(idx) {
                count += 1;
            }
        }
        ptr[row + 1] = ptr[row] + count;
    }

    let mut adj = vec![(0u32, 0u64); ptr[n]];
    for row in 0..n {
        let mut at = ptr[row];
        for idx in snap.row_ptr[row]..snap.row_ptr[row + 1] {
            if keep(idx) {
                adj[at] = (snap.col_idx[idx], snap.edge_id[idx]);
                at += 1;
            }
        }
        adj[ptr[row]..at].sort_unstable();
    }
    TypedSortedAdj { ptr, adj }
}

/// Transposed adjacency (edges grouped by destination) filtered to
/// `type_id`, rows sorted by `(src, edge id)`.
fn typed_in_sorted(snap: &CsrSnapshot, type_id: Option<TypeId>) -> TypedSortedAdj {
    let n = snap.dense_to_id.len();
    let keep = |idx: usize| type_id.is_none_or(|t| snap.edge_type[idx] == t);

    let mut ptr = vec![0usize; n + 1];
    for idx in 0..snap.col_idx.len() {
        if keep(idx) {
            ptr[snap.col_idx[idx] as usize + 1] += 1;
        }
    }
    for d in 0..n {
        ptr[d + 1] += ptr[d];
    }

    let mut at = ptr.clone();
    let mut adj = vec![(0u32, 0u64); ptr[n]];
    for row in 0..n {
        for idx in snap.row_ptr[row]..snap.row_ptr[row + 1] {
            if keep(idx) {
                let dst = snap.col_idx[idx] as usize;
                adj[at[dst]] = (row as u32, snap.edge_id[idx]);
                at[dst] += 1;
            }
        }
    }
    for d in 0..n {
        adj[ptr[d]..ptr[d + 1]].sort_unstable();
    }
    TypedSortedAdj { ptr, adj }
}

#[cfg(test)]
mod snapshot_freshness_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use std::collections::{BTreeMap, HashMap};

    use crate::Graph;
    use crate::graph::DegreeDirection;
    use crate::schema::NodeId;

    /// Canonicalize a component map to its underlying partition (each node mapped
    /// to the smallest node id in its component), so two results compare equal
    /// regardless of the component-id numbering.
    fn canonical_partition(cc: &HashMap<NodeId, u64>) -> BTreeMap<NodeId, NodeId> {
        let mut groups: HashMap<u64, Vec<NodeId>> = HashMap::new();
        for (&node, &comp) in cc {
            groups.entry(comp).or_default().push(node);
        }
        let mut out = BTreeMap::new();
        for members in groups.into_values() {
            let rep = *members.iter().min().unwrap();
            for n in members {
                out.insert(n, rep);
            }
        }
        out
    }

    /// A gate-driven refresh must produce what an explicit `rebuild_csr` would.
    /// Every kernel reads the snapshot, so this is the freshness proof for all of
    /// them: whatever the gate installed answers the same as a forced rebuild over
    /// the same LMDB state.
    #[test]
    fn gated_consumers_match_a_forced_rebuild() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let ids: Vec<NodeId> = (0..15)
            .map(|i| g.add_node("N", &json!({ "v": i })).unwrap())
            .collect();
        for i in 0..15 {
            g.add_edge(ids[i], ids[(i + 1) % 15], "R", &json!({}))
                .unwrap();
        }
        g.rebuild_csr().unwrap();

        // Mutations with no rebuild in between, including a brand-new node, so the
        // refresh has to extend the dense mapping as well as the rows.
        g.add_edge(ids[0], ids[7], "R", &json!({})).unwrap();
        let n15 = g.add_node("N", &json!({ "v": 15 })).unwrap();
        g.add_edge(ids[2], n15, "R", &json!({})).unwrap();
        g.add_edge(n15, ids[5], "R", &json!({})).unwrap();

        // Results through the freshness gate.
        let bfs_gated = g.bfs(ids[0], 3).unwrap();
        let deg_gated = g.degree_centrality(DegreeDirection::Both).unwrap();
        let cc_gated = canonical_partition(&g.connected_components().unwrap());

        // Results after a forced full rebuild over the same LMDB state.
        g.rebuild_csr().unwrap();
        let bfs_full = g.bfs(ids[0], 3).unwrap();
        let deg_full = g.degree_centrality(DegreeDirection::Both).unwrap();
        let cc_full = canonical_partition(&g.connected_components().unwrap());

        assert_eq!(bfs_gated, bfs_full, "bfs: gated vs forced rebuild");
        assert_eq!(deg_gated, deg_full, "degree: gated vs forced rebuild");
        assert_eq!(cc_gated, cc_full, "components: gated vs forced rebuild");
    }

    /// Freshness: a traversal reflects an edge, and a brand-new node reached through
    /// a new edge, with no explicit `rebuild_csr` between the write and the read.
    #[test]
    fn traversals_reflect_writes_without_an_explicit_rebuild() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        assert!(
            !g.bfs(a, 5).unwrap().contains(&b),
            "b is unreachable before the edge exists"
        );

        // Edge between existing nodes, no rebuild.
        g.add_edge(a, b, "R", &json!({})).unwrap();
        assert!(
            g.bfs(a, 1).unwrap().contains(&b),
            "b reachable from a after the edge, without a rebuild"
        );

        // A brand-new node reached through a new edge, still no rebuild: the refresh
        // has to grow the dense mapping, not just a row.
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(b, c, "R", &json!({})).unwrap();
        assert!(
            g.bfs(a, 2).unwrap().contains(&c),
            "new node c reachable two hops from a, without a rebuild"
        );
    }

    /// The same for a path search, which reads the CSR arrays rather than walking
    /// levels: one generation-gated refresh serves both.
    #[test]
    fn path_searches_reflect_writes_without_an_explicit_rebuild() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        assert!(
            g.all_paths(a, c).unwrap().is_empty(),
            "no path a..c before the edge exists"
        );

        // Edge b->c, no rebuild: the write-generation gate forces a refresh.
        g.add_edge(b, c, "R", &json!({})).unwrap();
        assert!(
            !g.all_paths(a, c).unwrap().is_empty(),
            "path a->b->c reflected without an explicit rebuild"
        );
    }

    /// Concurrent traversals interleaved with writes must each see one consistent
    /// snapshot. A reader takes the snapshot through an `ArcSwap` load, so a refresh
    /// installing a newer one mid-traversal cannot be observed halfway; a reader
    /// that re-read the pointer per hop could see a node set grow underneath it.
    #[test]
    fn concurrent_traversals_after_a_write_are_consistent() {
        use std::sync::Barrier;

        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();

        // A chain 0 -> 1 -> ... -> 29: bfs from node 0 reaches all 30 nodes.
        const N: usize = 30;
        let start = g.add_node("N", &json!({ "v": 0 })).unwrap();
        let mut prev = start;
        for i in 1..N {
            let node = g.add_node("N", &json!({ "v": i })).unwrap();
            g.add_edge(prev, node, "R", &json!({})).unwrap();
            prev = node;
        }
        g.rebuild_csr().unwrap();

        const THREADS: usize = 6;
        const ROUNDS: usize = 50;
        let mut expected = N;
        for r in 0..ROUNDS {
            // Attach a fresh node directly to `start`, so the reachable set grows by
            // exactly one and the expected count stays deterministic. Each round
            // leaves the snapshot stale, so the readers race on the refresh.
            let leaf = g.add_node("N", &json!({ "leaf": r })).unwrap();
            g.add_edge(start, leaf, "R", &json!({})).unwrap();
            expected += 1;

            let barrier = Barrier::new(THREADS);
            std::thread::scope(|s| {
                for _ in 0..THREADS {
                    let g = &g;
                    let barrier = &barrier;
                    s.spawn(move || {
                        // Synchronize so the threads hit the gate together.
                        barrier.wait();
                        let reached = g.bfs(start, u8::MAX).unwrap();
                        assert_eq!(
                            reached.len(),
                            expected,
                            "a concurrent traversal saw an inconsistent snapshot"
                        );
                    });
                }
            });
        }
    }

    /// Writers running concurrently with algorithm readers must not lose an update.
    /// A writer builds a star (every leaf edged to the center) large enough to cross
    /// the background-rebuild threshold, so the background rebuild runs concurrently
    /// with the readers' foreground refreshes. If a background pass installed a
    /// snapshot built before a write while claiming the later generation, the star
    /// would fracture into more than one connected component. It also exercises the
    /// maintenance lock for deadlock.
    #[test]
    fn concurrent_writes_and_reads_lose_no_edges() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Enough edges to cross REBUILD_THRESHOLD (1000) so a background rebuild
        // fires while readers are active.
        const LEAVES: usize = 1_500;

        let dir = TempDir::new().unwrap();
        let g = Arc::new(Graph::open(dir.path(), 1).unwrap());
        let center = g.add_node("N", &json!({ "c": true })).unwrap();
        g.rebuild_csr().unwrap();

        let done = Arc::new(AtomicBool::new(false));

        std::thread::scope(|s| {
            // Writer: attach each new leaf to the center.
            {
                let g = Arc::clone(&g);
                let done = Arc::clone(&done);
                s.spawn(move || {
                    for i in 0..LEAVES {
                        let leaf = g.add_node("N", &json!({ "leaf": i })).unwrap();
                        g.add_edge(center, leaf, "R", &json!({})).unwrap();
                    }
                    done.store(true, Ordering::Release);
                });
            }
            // Readers: hammer the gate from several kernels at once until the writer
            // is done.
            for _ in 0..4 {
                let g = Arc::clone(&g);
                let done = Arc::clone(&done);
                s.spawn(move || {
                    while !done.load(Ordering::Acquire) {
                        let _ = g.connected_components().unwrap();
                        let _ = g.bfs(center, 2).unwrap();
                        let _ = g.dfs(center, 2).unwrap();
                    }
                });
            }
        });

        // Every leaf is connected to the center, so the whole graph is one
        // connected component. A lost edge would leave that leaf isolated.
        let components = g.connected_components().unwrap();
        assert_eq!(components.len(), LEAVES + 1, "every node accounted for");
        let distinct: std::collections::HashSet<u64> = components.values().copied().collect();
        assert_eq!(
            distinct.len(),
            1,
            "the star must be one connected component; a fractured graph means a lost edge"
        );
    }
}

#[cfg(test)]
mod linear_path_count_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{Graph, PathCountSpec};

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    fn spec(
        rels: &[Option<&'static str>],
        labels: &[Option<&'static str>],
    ) -> PathCountSpec<'static> {
        PathCountSpec {
            rel_types: rels.to_vec(),
            labels: labels.to_vec(),
            vertex_allow: Vec::new(),
        }
    }

    /// A per-variable allow-set intersects with the label, restricting the
    /// counted paths to the supplied node ids exactly as a brute-force count
    /// over the same restriction does.
    #[test]
    fn two_hop_allow_set_restricts_middle_and_dest() {
        let (_dir, g) = open_tmp();
        // Five people; ages drive the allow-sets below.
        let p: Vec<_> = (0..5)
            .map(|i| {
                g.add_node("Person", &json!({ "age": 20 + i * 10 }))
                    .unwrap()
            })
            .collect();
        // A small FOLLOWS web with two-hop paths through several middles.
        let edges = [(0, 1), (0, 2), (1, 2), (1, 3), (2, 3), (2, 4), (3, 4)];
        for &(s, d) in &edges {
            g.add_edge(p[s], p[d], "FOLLOWS", &json!({})).unwrap();
        }

        // Allow middles {p1, p2} and destinations {p3, p4}. Brute-force the
        // count of (a)-[FOLLOWS]->(b)-[FOLLOWS]->(c) with b in the middle set
        // and c in the dest set.
        let mid = [p[1], p[2]];
        let dst = [p[3], p[4]];
        let mut expected = 0u64;
        for &(_s1, d1) in &edges {
            if !mid.contains(&p[d1]) {
                continue;
            }
            for &(s2, d2) in &edges {
                if p[s2] == p[d1] && dst.contains(&p[d2]) {
                    expected += 1;
                }
            }
        }
        assert!(expected > 0, "test graph must have qualifying paths");

        let filtered = PathCountSpec {
            rel_types: vec![Some("FOLLOWS"), Some("FOLLOWS")],
            labels: vec![Some("Person"), Some("Person"), Some("Person")],
            vertex_allow: vec![None, Some(mid.to_vec()), Some(dst.to_vec())],
        };
        assert_eq!(g.count_linear_paths(&filtered).unwrap(), expected);

        // The same pattern with no allow-sets counts every two-hop path, so the
        // restriction strictly reduces the count.
        let unfiltered = g
            .count_linear_paths(&spec(
                &[Some("FOLLOWS"), Some("FOLLOWS")],
                &[Some("Person"); 3],
            ))
            .unwrap();
        assert!(unfiltered > expected);
    }

    /// One hop counts typed edges whose endpoints carry the required labels.
    #[test]
    fn one_hop_counts_typed_edges() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(&[Some("KNOWS")], &[Some("Person"), Some("Person")]))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// A one-hop label predicate on the far endpoint excludes mismatched
    /// targets.
    #[test]
    fn one_hop_label_filter_excludes_endpoint() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("City", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(&[Some("KNOWS")], &[Some("Person"), Some("Person")]))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Two distinct hops over distinct nodes count once.
    #[test]
    fn two_hop_distinct_nodes_count_once() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(
                &[Some("KNOWS"), Some("KNOWS")],
                &[Some("Person"), Some("Person"), Some("Person")],
            ))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Parallel edges on one hop multiply the assignment count.
    #[test]
    fn two_hop_parallel_edges_multiply() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(
                &[Some("KNOWS"), Some("KNOWS")],
                &[Some("Person"), Some("Person"), Some("Person")],
            ))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// Relationship uniqueness removes the assignment where one self-loop edge
    /// would fill both hops, while keeping the path that leaves the self-loop.
    #[test]
    fn two_hop_self_loop_respects_relationship_uniqueness() {
        let (_dir, g) = open_tmp();
        let x = g.add_node("Person", &json!({})).unwrap();
        let y = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(x, x, "KNOWS", &json!({})).unwrap(); // self-loop
        g.add_edge(x, y, "KNOWS", &json!({})).unwrap();

        // Without the uniqueness rule the middle-node product would be 2
        // (in-degree 1 times out-degree 2 at x); the shared self-loop edge is
        // the one excluded pair, leaving the single (self-loop, x->y) path.
        let n = g
            .count_linear_paths(&spec(
                &[Some("KNOWS"), Some("KNOWS")],
                &[Some("Person"), Some("Person"), Some("Person")],
            ))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Several parallel self-loops at the middle node each remove exactly one
    /// assignment, the one where that edge fills both hops.
    ///
    /// The middle node's in-degree and out-degree both include every self-loop,
    /// so the raw product over-counts by the number of self-loops that satisfy
    /// both hops, not by one. This pins the correction as a count rather than a
    /// boolean "a self-loop exists" adjustment.
    #[test]
    fn two_hop_parallel_self_loops_each_remove_one_assignment() {
        let (_dir, g) = open_tmp();
        let w = g.add_node("Person", &json!({})).unwrap();
        let x = g.add_node("Person", &json!({})).unwrap();
        let y = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(w, x, "KNOWS", &json!({})).unwrap(); // in-edge
        g.add_edge(x, x, "KNOWS", &json!({})).unwrap(); // self-loop 1
        g.add_edge(x, x, "KNOWS", &json!({})).unwrap(); // self-loop 2 (parallel)
        g.add_edge(x, y, "KNOWS", &json!({})).unwrap(); // out-edge

        // Middle `x` has in-degree 3 and out-degree 3, so the raw product is 9.
        // The two self-loops are the only edges that could fill both hops, so
        // exactly two assignments are removed.
        let n = g
            .count_linear_paths(&spec(&[Some("KNOWS"), Some("KNOWS")], &[Some("Person"); 3]))
            .unwrap();
        assert_eq!(n, 7);
    }

    /// A self-loop is only excluded when its type satisfies both hops. With
    /// distinct per-hop types no single edge can fill both, so the product
    /// stands uncorrected.
    #[test]
    fn two_hop_self_loop_of_one_type_does_not_correct_a_mixed_type_pattern() {
        let (_dir, g) = open_tmp();
        let w = g.add_node("Person", &json!({})).unwrap();
        let x = g.add_node("Person", &json!({})).unwrap();
        let y = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(w, x, "KNOWS", &json!({})).unwrap();
        g.add_edge(x, x, "KNOWS", &json!({})).unwrap(); // self-loop, hop-1 type only
        g.add_edge(x, y, "LIKES", &json!({})).unwrap();

        // Hop 1 is KNOWS (in-edges of x: w->x and the self-loop, so 2), hop 2 is
        // LIKES (out-edges of x: x->y, so 1). The self-loop is not a LIKES edge,
        // so it cannot fill hop 2 and nothing is subtracted.
        let n = g
            .count_linear_paths(&spec(&[Some("KNOWS"), Some("LIKES")], &[Some("Person"); 3]))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// The kernel agrees with a brute-force enumeration of every `(r1, r2)`
    /// assignment on a graph that mixes self-loops, parallel edges, two
    /// relationship types, and an off-label endpoint.
    ///
    /// This is the differential guard for the counting path: the oracle applies
    /// relationship uniqueness by comparing edge ids directly, with no degree
    /// factorization, so any divergence in the factored kernel shows up here.
    #[test]
    fn two_hop_count_matches_brute_force_over_mixed_graph() {
        let (_dir, g) = open_tmp();
        let people: Vec<_> = (0..6)
            .map(|_| g.add_node("Person", &json!({})).unwrap())
            .collect();
        // One off-label node so the endpoint masks exclude real edges.
        let city = g.add_node("City", &json!({})).unwrap();

        // (src_index, dst_index, type); index 6 is the City node.
        let spec_edges: &[(usize, usize, &str)] = &[
            (0, 1, "KNOWS"),
            (0, 1, "KNOWS"), // parallel
            (1, 1, "KNOWS"), // self-loop at a middle
            (1, 1, "KNOWS"), // parallel self-loop
            (1, 2, "KNOWS"),
            (1, 2, "LIKES"),
            (2, 3, "KNOWS"),
            (2, 2, "LIKES"), // self-loop of the other type
            (3, 4, "KNOWS"),
            (4, 5, "KNOWS"),
            (5, 0, "KNOWS"),
            (1, 6, "KNOWS"), // into the City node
            (6, 2, "KNOWS"), // out of the City node
        ];
        let all: Vec<_> = people
            .iter()
            .copied()
            .chain(std::iter::once(city))
            .collect();
        let mut edges = Vec::new();
        for &(s, d, t) in spec_edges {
            let id = g.add_edge(all[s], all[d], t, &json!({})).unwrap();
            edges.push((all[s], all[d], t, id));
        }

        // Brute-force oracle: every ordered pair of distinct edges that chains
        // through a shared middle node, with all three endpoints on `Person`.
        let is_person = |n| n != city;
        for (t1, t2) in [
            (Some("KNOWS"), Some("KNOWS")),
            (Some("KNOWS"), Some("LIKES")),
            (Some("LIKES"), Some("KNOWS")),
            (None, None),
        ] {
            let mut expected = 0u64;
            for &(s1, d1, ty1, e1) in &edges {
                if t1.is_some_and(|t| t != ty1) || !is_person(s1) || !is_person(d1) {
                    continue;
                }
                for &(s2, d2, ty2, e2) in &edges {
                    if t2.is_some_and(|t| t != ty2) || !is_person(d2) {
                        continue;
                    }
                    // Chain through the middle, and relationship uniqueness.
                    if s2 == d1 && e2 != e1 {
                        expected += 1;
                    }
                }
            }
            let got = g
                .count_linear_paths(&spec(&[t1, t2], &[Some("Person"); 3]))
                .unwrap();
            assert_eq!(
                got, expected,
                "kernel disagreed with brute force for hops ({t1:?}, {t2:?})"
            );
        }
    }

    /// An unregistered relationship type matches nothing.
    #[test]
    fn unknown_relationship_type_counts_zero() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_linear_paths(&spec(&[Some("LIKES")], &[Some("Person"), Some("Person")]))
            .unwrap();
        assert_eq!(n, 0);
    }
}

#[cfg(test)]
mod triangle_cycle_count_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{Graph, TriangleCountSpec};

    fn open_tmp() -> (TempDir, Graph) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        (dir, g)
    }

    fn spec_all<'a>(rel: &'a str, label: &'a str) -> TriangleCountSpec<'a> {
        TriangleCountSpec {
            rel_types: [Some(rel); 3],
            labels: [Some(label); 3],
        }
    }

    /// One directed 3-cycle of distinct nodes matches once per rotation of
    /// `a`: three assignments, exactly what MATCH row semantics produce.
    #[test]
    fn single_cycle_counts_one_per_rotation() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 3);
    }

    /// A non-cycle triangle orientation (two edges out of one node) is not a
    /// directed cycle and must not count.
    #[test]
    fn non_cyclic_orientation_does_not_count() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, c, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Parallel edges are distinct relationships; doubling one hop doubles
    /// every assignment that uses it.
    #[test]
    fn parallel_edges_multiply() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 6);
    }

    /// Per-hop types are positional: a cycle whose third edge has a different
    /// type matches only the rotation whose hop order lines up with the spec.
    #[test]
    fn per_hop_types_are_positional() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "LIKES", &json!({})).unwrap();

        let homogeneous = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(homogeneous, 0);

        let mixed = g
            .count_triangle_cycles(&TriangleCountSpec {
                rel_types: [Some("KNOWS"), Some("KNOWS"), Some("LIKES")],
                labels: [Some("Person"); 3],
            })
            .unwrap();
        assert_eq!(mixed, 1);
    }

    /// Untyped hops match any relationship type.
    #[test]
    fn untyped_hops_match_any_type() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "LIKES", &json!({})).unwrap();
        g.add_edge(c, a, "FOLLOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&TriangleCountSpec {
                rel_types: [None; 3],
                labels: [Some("Person"); 3],
            })
            .unwrap();
        assert_eq!(n, 3);
    }

    /// A node missing the required label excludes every assignment that
    /// binds it; a multi-label node still qualifies.
    #[test]
    fn label_filter_applies_per_variable() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Robot", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        let strict = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(strict, 0);

        // With the label added, the node carries both labels and qualifies.
        g.add_label(c, "Person").unwrap();
        let after = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(after, 3);

        let unlabeled = g
            .count_triangle_cycles(&TriangleCountSpec {
                rel_types: [Some("KNOWS"); 3],
                labels: [None; 3],
            })
            .unwrap();
        assert_eq!(unlabeled, 3);
    }

    /// Relationship uniqueness: with `a == b == c` every hop is a self-loop,
    /// so matches are ordered triples of pairwise-distinct self-loop edges.
    /// Three self-loops give 3! = 6; two give none.
    #[test]
    fn self_loop_assignments_respect_relationship_uniqueness() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, a, "KNOWS", &json!({})).unwrap();
        g.add_edge(a, a, "KNOWS", &json!({})).unwrap();

        let two = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(two, 0);

        g.add_edge(a, a, "KNOWS", &json!({})).unwrap();
        let three = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(three, 6);
    }

    /// A self-loop combined with a 2-cycle yields one assignment per choice
    /// of the variable bound to the looped node: a=b, b=c, or c=a.
    #[test]
    fn self_loop_with_two_cycle_counts_each_position() {
        let (_dir, g) = open_tmp();
        let x = g.add_node("Person", &json!({})).unwrap();
        let y = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(x, x, "KNOWS", &json!({})).unwrap();
        g.add_edge(x, y, "KNOWS", &json!({})).unwrap();
        g.add_edge(y, x, "KNOWS", &json!({})).unwrap();

        let n = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(n, 3);
    }

    /// Unknown relationship types and labels match nothing instead of
    /// erroring: the query layer maps absent registry entries to empty scans.
    #[test]
    fn unknown_type_or_label_counts_zero() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();
        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();

        assert_eq!(
            g.count_triangle_cycles(&spec_all("NOPE", "Person"))
                .unwrap(),
            0
        );
        assert_eq!(
            g.count_triangle_cycles(&spec_all("KNOWS", "Ghost"))
                .unwrap(),
            0
        );
    }

    /// The count must reflect committed writes without an explicit
    /// `rebuild_csr`: the freshness gate covers this consumer.
    #[test]
    fn count_is_fresh_after_writes() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("Person", &json!({})).unwrap();
        let b = g.add_node("Person", &json!({})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        g.add_edge(a, b, "KNOWS", &json!({})).unwrap();
        g.add_edge(b, c, "KNOWS", &json!({})).unwrap();

        let before = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(before, 0);

        g.add_edge(c, a, "KNOWS", &json!({})).unwrap();
        let after = g
            .count_triangle_cycles(&spec_all("KNOWS", "Person"))
            .unwrap();
        assert_eq!(after, 3);
    }

    /// An empty graph counts zero without erroring on unmaterialized state.
    #[test]
    fn empty_graph_counts_zero() {
        let (_dir, g) = open_tmp();
        assert_eq!(
            g.count_triangle_cycles(&spec_all("KNOWS", "Person"))
                .unwrap(),
            0
        );
    }

    /// Changing an edge's weight through `update_edge` must be reflected by the
    /// next weighted shortest path. The snapshot's weights have no incremental
    /// maintenance, so `update_edge` must advance the write generation to force a
    /// rebuild; otherwise the stale array serves the old weight, or (with a changed
    /// weight the reconstruction can no longer match) no path at all.
    #[test]
    fn update_edge_weight_refreshes_dijkstra() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        // Direct a->b costs 1; the detour a->c->b costs 10.
        let direct = g.add_edge(a, b, "R", &json!({ "weight": 1.0 })).unwrap();
        g.add_edge(a, c, "R", &json!({ "weight": 5.0 })).unwrap();
        g.add_edge(c, b, "R", &json!({ "weight": 5.0 })).unwrap();
        g.rebuild_csr().unwrap();
        assert_eq!(
            g.shortest_path_dijkstra(a, b)
                .unwrap()
                .unwrap()
                .total_weight,
            1.0
        );

        // Make the direct edge expensive: the detour is now the shortest path.
        g.update_edge(direct, &json!({ "weight": 100.0 })).unwrap();
        let p = g
            .shortest_path_dijkstra(a, b)
            .unwrap()
            .expect("a path a->b still exists after update_edge");
        assert_eq!(p.total_weight, 10.0, "update_edge weight must be honored");
        assert_eq!(p.nodes, vec![a, c, b]);
    }

    /// The public algorithm entry points must gate themselves, and must do it
    /// without deadlocking.
    ///
    /// Each can be called on a graph that has built nothing, so each reaches its
    /// gate, and the gate takes a non-reentrant mutex before rebuilding. A gate that
    /// acquired it twice, or that recursed into a sibling gate while holding it,
    /// would hang rather than fail. Each call runs on its own thread with a
    /// deadline, so a regression fails this test instead of hanging the suite.
    #[test]
    fn public_entry_points_gate_themselves_without_deadlocking() {
        fn within_deadline<T: Send + 'static>(
            what: &str,
            f: impl FnOnce() -> T + Send + 'static,
        ) -> T {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(f());
            });
            rx.recv_timeout(std::time::Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("{what} did not return: it deadlocked on its own gate"))
        }

        // A fresh graph builds nothing, so each entry point hits the unbuilt path.
        let dir = TempDir::new().unwrap();
        let g = std::sync::Arc::new(Graph::open(dir.path(), 1).unwrap());
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        g.add_edge(a, b, "R", &json!({ "weight": 2.0 })).unwrap();

        let g1 = g.clone();
        let ranks = within_deadline("page_rank on an unbuilt graph", move || {
            g1.page_rank(3, 0.85)
        })
        .expect("PageRank must build what it needs");
        assert_eq!(ranks.len(), 2);

        let g2 = g.clone();
        let path_found = within_deadline("shortest_path on an unbuilt graph", move || {
            g2.shortest_path(a, b)
        })
        .expect("shortest path must build what it needs");
        assert_eq!(path_found, Some(vec![a, b]));

        // The weighted gate is the one that installs a different kind of snapshot,
        // so it gets its own unbuilt-graph pass.
        let dir2 = TempDir::new().unwrap();
        let g3 = std::sync::Arc::new(Graph::open(dir2.path(), 1).unwrap());
        let c = g3.add_node("N", &json!({})).unwrap();
        let d = g3.add_node("N", &json!({})).unwrap();
        g3.add_edge(c, d, "R", &json!({ "weight": 4.0 })).unwrap();
        let g4 = g3.clone();
        let weighted = within_deadline("shortest_path_dijkstra on an unbuilt graph", move || {
            g4.shortest_path_dijkstra(c, d)
        })
        .expect("Dijkstra must build the weighted snapshot it needs");
        assert_eq!(weighted.unwrap().total_weight, 4.0);
    }

    /// A weighted consumer must get real weights on a graph whose snapshot was
    /// already built by an unweighted one, and a later unweighted consumer must not
    /// strip them back off.
    ///
    /// An unweighted build carries no weights and is current at its generation, so a
    /// gate that only compared generations would hand Dijkstra a snapshot with none.
    /// Running `bfs` first and then Dijkstra with no write in between is exactly that
    /// sequence. The second half pins the stickiness: without it the two consumers
    /// would rebuild in turn, once each per write.
    #[test]
    fn a_weighted_consumer_gets_weights_after_an_unweighted_build() {
        let (_dir, g) = open_tmp();
        let a = g.add_node("N", &json!({})).unwrap();
        let b = g.add_node("N", &json!({})).unwrap();
        let c = g.add_node("N", &json!({})).unwrap();
        // The detour is cheaper than the direct edge, so a default weight of 1.0 per
        // edge would pick the direct edge and score it 1.0.
        g.add_edge(a, b, "R", &json!({ "weight": 100.0 })).unwrap();
        g.add_edge(a, c, "R", &json!({ "weight": 1.0 })).unwrap();
        g.add_edge(c, b, "R", &json!({ "weight": 1.0 })).unwrap();

        // An unweighted consumer builds first.
        assert!(!g.bfs(a, 2).unwrap().is_empty());
        assert!(
            g.csr_cache.snapshot.load().edge_weight.is_none(),
            "bfs must not have paid the weights scan"
        );

        let p = g
            .shortest_path_dijkstra(a, b)
            .unwrap()
            .expect("a path a->b exists");
        assert_eq!(p.total_weight, 2.0, "the weights must be the stored ones");
        assert_eq!(p.nodes, vec![a, c, b]);

        // The request is sticky, so an unweighted consumer running afterwards, and a
        // rebuild triggered by a write, both keep the weights loaded.
        assert!(!g.bfs(a, 2).unwrap().is_empty());
        g.add_edge(b, c, "R", &json!({ "weight": 1.0 })).unwrap();
        assert!(!g.bfs(a, 2).unwrap().is_empty());
        assert!(
            g.csr_cache.snapshot.load().edge_weight.is_some(),
            "an unweighted consumer must not strip weights another one asked for"
        );
    }

    /// PageRank must not depend on whether a prior bulk typed expansion refreshed
    /// the snapshot.
    #[test]
    fn page_rank_fresh_after_a_bulk_expansion_refresh() {
        let (_dir, g) = open_tmp();
        let mut nodes = Vec::new();
        for _ in 0..70 {
            nodes.push(g.add_node("N", &json!({})).unwrap());
        }
        g.rebuild_csr().unwrap();
        for w in nodes.windows(2) {
            g.add_edge(w[0], w[1], "R", &json!({})).unwrap();
        }
        // A bulk typed expansion over more than `STALE_POINT_EXPAND_MAX` sources
        // refreshes the snapshot as a side effect.
        let _ = g.expand_bulk(&nodes, Some("R"), false).unwrap();
        let gated = g.page_rank(20, 0.85).unwrap();

        g.rebuild_csr().unwrap();
        let full = g.page_rank(20, 0.85).unwrap();

        for n in &nodes {
            assert!(
                (gated[n] - full[n]).abs() < 1e-6,
                "page_rank for {n} diverges after a bulk-expansion refresh: {} vs {}",
                gated[n],
                full[n]
            );
        }
    }
}

#[cfg(test)]
mod snapshot_only_gate_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{Graph, GroupedDegreeSpec, PathCountSpec, TriangleCountSpec, schema::NodeId};

    /// A triangle plus a disjoint two-edge chain, closed and reopened so the
    /// handle starts with nothing materialized. Every expected count below is
    /// non-zero, so reading an empty snapshot fails the assertion rather than
    /// coincidentally matching.
    fn seeded_dir() -> (TempDir, Vec<NodeId>) {
        let dir = TempDir::new().unwrap();
        let ids = {
            let g = Graph::open(dir.path(), 1).unwrap();
            let ids: Vec<_> = (0..6)
                .map(|i| g.add_node("Person", &json!({ "n": i })).unwrap())
                .collect();
            for &(s, d) in &[(0, 1), (1, 2), (2, 0), (3, 4), (4, 5)] {
                g.add_edge(ids[s], ids[d], "FOLLOWS", &json!({})).unwrap();
            }
            ids
        };
        (dir, ids)
    }

    fn one_hop() -> PathCountSpec<'static> {
        PathCountSpec {
            rel_types: vec![Some("FOLLOWS")],
            labels: vec![Some("Person"), Some("Person")],
            vertex_allow: Vec::new(),
        }
    }

    fn two_hop() -> PathCountSpec<'static> {
        PathCountSpec {
            rel_types: vec![Some("FOLLOWS"), Some("FOLLOWS")],
            labels: vec![Some("Person"); 3],
            vertex_allow: Vec::new(),
        }
    }

    /// The counting kernels read the adjacency arrays alone, so they must build no
    /// more than that. The per-edge weights are the one extra a snapshot can carry,
    /// and loading them costs a second full scan of `edges` that no counting kernel
    /// ever reads.
    #[test]
    fn count_kernels_serve_from_the_snapshot_without_loading_weights() {
        let (dir, _ids) = seeded_dir();

        {
            let g = Graph::open(dir.path(), 1).unwrap();
            assert_eq!(g.count_linear_paths(&one_hop()).unwrap(), 5);
            assert!(
                g.csr_cache.snapshot.load().edge_weight.is_none(),
                "a one-hop count must not pay the weights scan"
            );
        }
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            assert_eq!(g.count_linear_paths(&two_hop()).unwrap(), 4);
            assert!(
                g.csr_cache.snapshot.load().edge_weight.is_none(),
                "a two-hop count must not pay the weights scan"
            );
        }
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            let spec = TriangleCountSpec {
                rel_types: [Some("FOLLOWS"); 3],
                labels: [Some("Person"); 3],
            };
            // One directed 3-cycle, counted once per rotation of `a`.
            assert_eq!(g.count_triangle_cycles(&spec).unwrap(), 3);
            assert!(
                g.csr_cache.snapshot.load().edge_weight.is_none(),
                "a triangle count must not pay the weights scan"
            );
        }
        {
            let g = Graph::open(dir.path(), 1).unwrap();
            let spec = GroupedDegreeSpec {
                rel_type: Some("FOLLOWS"),
                group_is_dst: false,
                group_label: Some("Person"),
                counted_label: Some("Person"),
                counted_allow: None,
                counted_nonnull_prop: None,
            };
            let counts = g.grouped_edge_counts(&spec).unwrap();
            // Five sources each with out-degree one; the sixth node has none.
            assert_eq!(counts.len(), 5);
            assert!(counts.iter().all(|&(_, c)| c == 1));
            assert!(
                g.csr_cache.snapshot.load().edge_weight.is_none(),
                "a grouped degree count must not pay the weights scan"
            );
        }
    }

    /// Narrowing the gate must not weaken freshness: a kernel run after a write
    /// in the same session observes that write, because the snapshot gate
    /// rebuilds on the `write_gen` versus `snapshot_gen` mismatch.
    #[test]
    fn count_kernels_observe_writes_made_after_the_first_count() {
        let (dir, ids) = seeded_dir();
        let g = Graph::open(dir.path(), 1).unwrap();

        assert_eq!(g.count_linear_paths(&one_hop()).unwrap(), 5);
        assert_eq!(g.count_linear_paths(&two_hop()).unwrap(), 4);

        // Close the disjoint chain into the triangle's tail: 5 -> 3 adds one
        // edge, one new two-hop path through 3 (5->3->4), and no new triangle.
        g.add_edge(ids[5], ids[3], "FOLLOWS", &json!({})).unwrap();

        assert_eq!(
            g.count_linear_paths(&one_hop()).unwrap(),
            6,
            "the one-hop count must include the edge added after the first count"
        );
        assert_eq!(
            g.count_linear_paths(&two_hop()).unwrap(),
            6,
            "5->3 adds 4->5->3 and 5->3->4"
        );
        assert!(
            g.csr_cache.snapshot.load().edge_weight.is_none(),
            "refreshing the snapshot must not drag in the weights scan"
        );
    }

    /// A node deletion reshuffles the dense mapping, so the snapshot gate must
    /// still produce correct counts afterwards.
    #[test]
    fn count_kernels_are_correct_after_a_node_deletion() {
        let (dir, ids) = seeded_dir();
        let g = Graph::open(dir.path(), 1).unwrap();
        assert_eq!(g.count_linear_paths(&one_hop()).unwrap(), 5);

        // Deleting node 5 drops the 4->5 edge with it.
        g.delete_node(ids[5]).unwrap();

        assert_eq!(g.count_linear_paths(&one_hop()).unwrap(), 4);
        assert_eq!(
            g.count_linear_paths(&two_hop()).unwrap(),
            3,
            "only the triangle's three two-hop paths remain"
        );
    }
}

#[cfg(test)]
mod typed_neighbor_count_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{Graph, NeighborCountSpec, schema::NodeId};

    /// `a` follows `b` twice (parallel edges), `c` once, and itself once; `b`
    /// follows `c`. `c` carries no `tag`, and `d` is a differently labeled node
    /// `a` also follows.
    fn fixture() -> (TempDir, Graph, Vec<NodeId>) {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let a = g.add_node("Person", &json!({"tag": "a"})).unwrap();
        let b = g.add_node("Person", &json!({"tag": "b"})).unwrap();
        let c = g.add_node("Person", &json!({})).unwrap();
        let d = g.add_node("Robot", &json!({"tag": "d"})).unwrap();
        for (s, t) in [(a, b), (a, b), (a, c), (a, a), (b, c)] {
            g.add_edge(s, t, "FOLLOWS", &json!({})).unwrap();
        }
        g.add_edge(a, d, "FOLLOWS", &json!({})).unwrap();
        g.add_edge(a, b, "BLOCKS", &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        (dir, g, vec![a, b, c, d])
    }

    fn spec<'a>(
        rel_type: Option<&'a str>,
        incoming: bool,
        labels: &'a [&'a str],
        nonnull: Option<&'a str>,
    ) -> NeighborCountSpec<'a> {
        NeighborCountSpec {
            rel_type,
            incoming,
            neighbor_labels: labels,
            neighbor_allow: None,
            neighbor_nonnull_prop: nonnull,
        }
    }

    /// A `count(v.prop)` pass over a handful of neighbors must not build the
    /// property columns.
    ///
    /// Presence is resolved for the neighbors the walk actually reaches, through
    /// the same small-request path a point read uses. Resolving it from the columns
    /// instead forced one full node scan with a msgpack decode per node, which on a
    /// large graph is the dominant cost and the one a lazily opened graph exists to
    /// defer; the fallback this kernel replaces never paid it.
    #[test]
    fn a_nonnull_count_over_few_neighbors_does_not_build_the_columns() {
        let (_dir, g, ids) = fixture();
        let (a, b) = (ids[0], ids[1]);
        assert!(
            !g.prop_columns.is_built(),
            "the fixture must start with the columns absent"
        );

        // a's Person FOLLOWS neighbors are b, b, c, and a. Only c lacks `tag`, so
        // four qualify and three count.
        let counts = g
            .typed_neighbor_counts(
                &[a, b],
                &spec(Some("FOLLOWS"), false, &["Person"], Some("tag")),
            )
            .unwrap();
        assert_eq!(counts, vec![(4, 3), (1, 0)]);

        assert!(
            !g.prop_columns.is_built(),
            "resolving presence for a few neighbors must not build every column"
        );
    }

    /// Presence must come from the nodes the walk reached, not from a whole-column
    /// summary trusted because its length matched the snapshot's node count.
    ///
    /// The previous shortcut skipped the mask when the column had no nulls and its
    /// length equalled the snapshot's node count. Equal counts do not imply equal
    /// node sets, so a deletion plus an insertion between the snapshot refresh and
    /// the column refresh passed that test while the snapshot still held a node the
    /// columns had never seen, and that node's edges counted as non-null. Here the
    /// property is present on every node that has it, which is exactly the state
    /// that used to trigger the shortcut, and the node missing it must still be
    /// excluded from the count.
    #[test]
    fn presence_is_resolved_per_visited_neighbor() {
        let (_dir, g, ids) = fixture();
        let a = ids[0];

        // Force the columns to exist and to hold no nulls for `tag` among the
        // nodes that carry it, the state the old length-plus-all-present shortcut
        // recognized. Asked for directly: no reader builds them unconditionally, so
        // a grouped read over this many ids is served without them.
        g.prop_columns
            .with_fresh(&g.storage, |_| ())
            .expect("materialize the property columns");
        assert!(g.prop_columns.is_built());

        let counts = g
            .typed_neighbor_counts(
                &[a],
                &spec(Some("FOLLOWS"), false, &["Person"], Some("tag")),
            )
            .unwrap();
        assert_eq!(
            counts,
            vec![(4, 3)],
            "the neighbor with no `tag` must not be counted"
        );
    }

    /// An allow-set narrows the count on top of the labels, an empty one admits
    /// nothing, and a member absent from the graph is simply never reached.
    #[test]
    fn allow_set_intersects_with_the_labels() {
        let (_dir, g, ids) = fixture();
        let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);

        // a's five FOLLOWS neighbors are b, b, c, a, d. Allowing only b and c
        // keeps the two b edges and the one c edge.
        let allow = [b, c];
        let narrowed = NeighborCountSpec {
            rel_type: Some("FOLLOWS"),
            incoming: false,
            neighbor_labels: &[],
            neighbor_allow: Some(&allow),
            neighbor_nonnull_prop: None,
        };
        assert_eq!(
            g.typed_neighbor_counts(&[a], &narrowed).unwrap(),
            vec![(3, 3)]
        );

        // Intersected with the Person label, d drops out anyway; allowing only d
        // then leaves nothing.
        let allow_d = [d];
        let with_label = NeighborCountSpec {
            rel_type: Some("FOLLOWS"),
            incoming: false,
            neighbor_labels: &["Person"],
            neighbor_allow: Some(&allow_d),
            neighbor_nonnull_prop: None,
        };
        assert_eq!(
            g.typed_neighbor_counts(&[a], &with_label).unwrap(),
            vec![(0, 0)]
        );

        // An empty allow-set admits no neighbor; an unknown id is inert.
        let empty = NeighborCountSpec {
            rel_type: Some("FOLLOWS"),
            incoming: false,
            neighbor_labels: &[],
            neighbor_allow: Some(&[]),
            neighbor_nonnull_prop: None,
        };
        assert_eq!(g.typed_neighbor_counts(&[a], &empty).unwrap(), vec![(0, 0)]);
        let unknown = [b, d + 9999];
        let with_unknown = NeighborCountSpec {
            rel_type: Some("FOLLOWS"),
            incoming: false,
            neighbor_labels: &[],
            neighbor_allow: Some(&unknown),
            neighbor_nonnull_prop: None,
        };
        assert_eq!(
            g.typed_neighbor_counts(&[a], &with_unknown).unwrap(),
            vec![(2, 2)]
        );

        // The non-null filter still narrows only the counted total: c has no tag.
        let allow_bc = [b, c];
        let tagged = NeighborCountSpec {
            rel_type: Some("FOLLOWS"),
            incoming: false,
            neighbor_labels: &[],
            neighbor_allow: Some(&allow_bc),
            neighbor_nonnull_prop: Some("tag"),
        };
        assert_eq!(
            g.typed_neighbor_counts(&[a], &tagged).unwrap(),
            vec![(3, 2)]
        );
    }

    /// `adjacency_span` totals the sources' adjacency rows without narrowing by
    /// type, so it bounds the edges a count would visit for them.
    #[test]
    fn adjacency_span_bounds_the_visited_edges() {
        let (_dir, g, ids) = fixture();
        let (a, b, d) = (ids[0], ids[1], ids[3]);

        // a has 5 FOLLOWS plus 1 BLOCKS out; b has 1 out.
        assert_eq!(g.adjacency_span(&[a], false).unwrap(), 6);
        assert_eq!(g.adjacency_span(&[a, b], false).unwrap(), 7);
        // Incoming: a from its self-loop; b from a twice by FOLLOWS and once by
        // BLOCKS; d once.
        assert_eq!(g.adjacency_span(&[a], true).unwrap(), 1);
        assert_eq!(g.adjacency_span(&[b], true).unwrap(), 3);
        assert_eq!(g.adjacency_span(&[d], true).unwrap(), 1);

        // The span is at least the typed count it bounds.
        let typed = g
            .typed_neighbor_counts(&[a, b], &spec(Some("FOLLOWS"), false, &[], None))
            .unwrap();
        let counted: u64 = typed.iter().map(|(q, _)| q).sum();
        assert!(counted <= g.adjacency_span(&[a, b], false).unwrap());

        // Unknown sources and an empty list contribute nothing.
        assert_eq!(g.adjacency_span(&[d + 9999], false).unwrap(), 0);
        assert_eq!(g.adjacency_span(&[], false).unwrap(), 0);
    }

    /// Outgoing and incoming counts, with parallel edges counted per edge and a
    /// self-loop counted once for its source.
    #[test]
    fn counts_each_edge_in_both_directions() {
        let (_dir, g, ids) = fixture();
        let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);

        // a: b twice, c, a (self-loop), d = 5 FOLLOWS out; b: c = 1.
        let out = g
            .typed_neighbor_counts(&[a, b, c, d], &spec(Some("FOLLOWS"), false, &[], None))
            .unwrap();
        assert_eq!(out, vec![(5, 5), (1, 1), (0, 0), (0, 0)]);

        // Incoming: a from itself; b from a twice; c from a and b; d from a.
        let inc = g
            .typed_neighbor_counts(&[a, b, c, d], &spec(Some("FOLLOWS"), true, &[], None))
            .unwrap();
        assert_eq!(inc, vec![(1, 1), (2, 2), (2, 2), (1, 1)]);

        // Untyped follows every type, adding a's BLOCKS edge.
        let any = g
            .typed_neighbor_counts(&[a], &spec(None, false, &[], None))
            .unwrap();
        assert_eq!(any, vec![(6, 6)]);
    }

    /// A neighbor label narrows the count, a conjunction of labels intersects,
    /// and an unknown label or relationship type counts zero.
    #[test]
    fn labels_and_types_narrow_the_count() {
        let (_dir, g, ids) = fixture();
        let (a, b) = (ids[0], ids[1]);
        g.add_label(b, "Vip").unwrap();
        g.rebuild_csr().unwrap();

        // Of a's five FOLLOWS neighbors, four are Person (b, b, c, a) and d is not.
        let person = g
            .typed_neighbor_counts(&[a], &spec(Some("FOLLOWS"), false, &["Person"], None))
            .unwrap();
        assert_eq!(person, vec![(4, 4)]);

        // Person AND Vip is only b, reached twice.
        let vip = g
            .typed_neighbor_counts(
                &[a],
                &spec(Some("FOLLOWS"), false, &["Person", "Vip"], None),
            )
            .unwrap();
        assert_eq!(vip, vec![(2, 2)]);

        for unknown in [
            spec(Some("NOPE"), false, &[], None),
            spec(Some("FOLLOWS"), false, &["Nope"], None),
        ] {
            assert_eq!(
                g.typed_neighbor_counts(&[a], &unknown).unwrap(),
                vec![(0, 0)]
            );
        }
    }

    /// `neighbor_nonnull_prop` leaves the qualifying total alone and narrows only
    /// the counted total, so a source whose every neighbor lacks the property
    /// still reports rows with a zero count. An absent property counts zero.
    #[test]
    fn nonnull_property_narrows_only_the_counted_total() {
        let (_dir, g, ids) = fixture();
        let (a, b) = (ids[0], ids[1]);

        // a's Person neighbors are b, b, c, a; only c has no `tag`.
        let tagged = g
            .typed_neighbor_counts(
                &[a],
                &spec(Some("FOLLOWS"), false, &["Person"], Some("tag")),
            )
            .unwrap();
        assert_eq!(tagged, vec![(4, 3)]);

        // b's only FOLLOWS neighbor is c, which has no `tag`: one row, count zero.
        let untagged = g
            .typed_neighbor_counts(&[b], &spec(Some("FOLLOWS"), false, &[], Some("tag")))
            .unwrap();
        assert_eq!(untagged, vec![(1, 0)]);

        // A property no node carries counts zero everywhere.
        let absent = g
            .typed_neighbor_counts(&[a], &spec(Some("FOLLOWS"), false, &[], Some("nope")))
            .unwrap();
        assert_eq!(absent, vec![(5, 0)]);
    }

    /// Variables sharing a label share the *scan* that builds their mask, not the
    /// mask itself: a pushed-down allow-set intersects into one variable's mask in
    /// place, so aliasing them would leak one variable's filter onto another. This
    /// pins that by giving three same-labelled variables different allow-sets.
    #[test]
    fn same_label_variables_do_not_share_a_mutated_mask() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        let ids: Vec<NodeId> = (0..4)
            .map(|i| g.add_node("P", &json!({ "k": i })).unwrap())
            .collect();
        // A 3-chain plus a shortcut, so several two-hop paths exist.
        for &(a, b) in &[(0, 1), (1, 2), (2, 3), (0, 2)] {
            g.add_edge(ids[a], ids[b], "F", &json!({})).unwrap();
        }
        g.rebuild_csr().unwrap();

        // Unfiltered: 0->1->2, 1->2->3, 0->2->3.
        let base = crate::PathCountSpec {
            rel_types: vec![Some("F"), Some("F")],
            labels: vec![Some("P"); 3],
            vertex_allow: Vec::new(),
        };
        assert_eq!(g.count_linear_paths(&base).unwrap(), 3);

        // Restricting only the middle variable to node 2 keeps both paths through
        // it (1->2->3 and 0->2->3) and drops 0->1->2, whose middle is node 1. If
        // the three same-labelled variables shared one mask, this intersection
        // would also constrain the endpoints and the count would fall further.
        let middle_only = crate::PathCountSpec {
            rel_types: vec![Some("F"), Some("F")],
            labels: vec![Some("P"); 3],
            vertex_allow: vec![None, Some(vec![ids[2]]), None],
        };
        assert_eq!(
            g.count_linear_paths(&middle_only).unwrap(),
            2,
            "an allow-set on the middle variable must not constrain the others"
        );

        // A different allow-set per same-labelled variable: source in {0}, middle
        // in {1,2}, destination in {2}. Only 0->1->2 satisfies all three.
        let per_variable = crate::PathCountSpec {
            rel_types: vec![Some("F"), Some("F")],
            labels: vec![Some("P"); 3],
            vertex_allow: vec![
                Some(vec![ids[0]]),
                Some(vec![ids[1], ids[2]]),
                Some(vec![ids[2]]),
            ],
        };
        assert_eq!(g.count_linear_paths(&per_variable).unwrap(), 1);
    }

    /// The non-null filter agrees whether the mask is built or skipped. A column
    /// with no nulls anywhere takes the skip, one with a null takes the mask, and
    /// both must count the same edges.
    #[test]
    fn nonnull_filter_agrees_when_the_mask_is_skipped() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        // Every node carries `k`, so the column has no nulls.
        let ids: Vec<NodeId> = (0..4)
            .map(|i| g.add_node("P", &json!({ "k": i })).unwrap())
            .collect();
        for &(s, d) in &[(0, 1), (0, 2), (0, 3), (1, 2)] {
            g.add_edge(ids[s], ids[d], "F", &json!({})).unwrap();
        }
        g.rebuild_csr().unwrap();

        // All-present: the mask is skipped, so the counted total equals the
        // unfiltered one.
        let unfiltered = g
            .typed_neighbor_counts(&ids, &spec(Some("F"), false, &[], None))
            .unwrap();
        let all_present = g
            .typed_neighbor_counts(&ids, &spec(Some("F"), false, &[], Some("k")))
            .unwrap();
        assert_eq!(
            unfiltered, all_present,
            "an all-present column filters nothing"
        );
        assert_eq!(unfiltered[0], (3, 3), "the first node has three neighbors");

        // Drop `k` from one neighbor: the column now has a null, so the mask is
        // built and the count follows it.
        g.update_node(ids[2], &json!({})).unwrap();
        g.rebuild_csr().unwrap();
        let with_null = g
            .typed_neighbor_counts(&ids, &spec(Some("F"), false, &[], Some("k")))
            .unwrap();
        assert_eq!(
            with_null[0],
            (3, 2),
            "the neighbor that lost `k` no longer counts"
        );
        // Existence is unchanged: only the counted total narrows.
        assert_eq!(
            g.typed_neighbor_counts(&ids, &spec(Some("F"), false, &[], None))
                .unwrap(),
            unfiltered
        );
    }

    /// Input order is preserved, duplicate sources each get their own entry, and
    /// a source absent from the graph counts zero rather than erroring.
    #[test]
    fn preserves_input_order_and_tolerates_unknown_sources() {
        let (_dir, g, ids) = fixture();
        let (a, b) = (ids[0], ids[1]);
        let missing = ids[3] + 9999;

        let out = g
            .typed_neighbor_counts(
                &[b, missing, a, b],
                &spec(Some("FOLLOWS"), false, &[], None),
            )
            .unwrap();
        assert_eq!(out, vec![(1, 1), (0, 0), (5, 5), (1, 1)]);

        assert!(
            g.typed_neighbor_counts(&[], &spec(None, false, &[], None))
                .unwrap()
                .is_empty()
        );
    }

    /// The kernel agrees with counting a materialized expansion edge by edge,
    /// over random multigraphs with self-loops, parallel edges, mixed labels,
    /// and a property some nodes lack.
    #[test]
    fn matches_a_materialized_expansion() {
        use proptest::prelude::*;

        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 32,
            ..ProptestConfig::default()
        });
        let strategy = (
            1usize..=6,
            proptest::collection::vec((0usize..6, 0usize..6), 0..24),
            proptest::collection::vec(any::<bool>(), 6),
            proptest::collection::vec(any::<bool>(), 6),
        );
        runner
            .run(&strategy, |(n_nodes, edges, has_tag, is_vip)| {
                let dir = TempDir::new().unwrap();
                let g = Graph::open(dir.path(), 1).unwrap();
                let ids: Vec<NodeId> = (0..n_nodes)
                    .map(|i| {
                        let props = if has_tag[i] {
                            json!({ "tag": i as i64 })
                        } else {
                            json!({})
                        };
                        if is_vip[i] {
                            g.add_node_multi(&["Person", "Vip"], &props).unwrap()
                        } else {
                            g.add_node("Person", &props).unwrap()
                        }
                    })
                    .collect();
                for (s, d) in &edges {
                    if *s < n_nodes && *d < n_nodes {
                        g.add_edge(ids[*s], ids[*d], "F", &json!({})).unwrap();
                    }
                }
                g.rebuild_csr().unwrap();

                for incoming in [false, true] {
                    for labels in [&[][..], &["Vip"][..]] {
                        for nonnull in [None, Some("tag")] {
                            let spec = NeighborCountSpec {
                                rel_type: Some("F"),
                                incoming,
                                neighbor_labels: labels,
                                neighbor_allow: None,
                                neighbor_nonnull_prop: nonnull,
                            };
                            let got = g.typed_neighbor_counts(&ids, &spec).unwrap();
                            for (i, &src) in ids.iter().enumerate() {
                                // Oracle: enumerate the source's edges directly.
                                let neighbors: Vec<NodeId> = if incoming {
                                    g.in_neighbors(src)
                                        .unwrap()
                                        .into_iter()
                                        .map(|e| e.node)
                                        .collect()
                                } else {
                                    g.out_neighbors(src)
                                        .unwrap()
                                        .into_iter()
                                        .map(|e| e.node)
                                        .collect()
                                };
                                let mut qualifying = 0u64;
                                let mut counted = 0u64;
                                for nb in neighbors {
                                    let idx = ids.iter().position(|x| *x == nb).unwrap();
                                    if !labels.is_empty() && !is_vip[idx] {
                                        continue;
                                    }
                                    qualifying += 1;
                                    if nonnull.is_none() || has_tag[idx] {
                                        counted += 1;
                                    }
                                }
                                assert_eq!(
                                    got[i],
                                    (qualifying, counted),
                                    "source {i}, incoming={incoming}, labels={labels:?}, \
                                     nonnull={nonnull:?}"
                                );
                            }
                        }
                    }
                }
                Ok(())
            })
            .unwrap();
    }
}

#[cfg(test)]
mod parallel_kernel_tests {
    use serde_json::json;
    use tempfile::TempDir;

    use crate::{Graph, PathCountSpec, graph::algo::FORCE_KERNEL_THREADS, schema::NodeId};

    /// Splitting a counting kernel across threads must not change its result. The
    /// two-hop count reduces over disjoint ranges of the middle node, so this
    /// drives the same graph at one thread and at several and compares, including
    /// the self-loop correction that relationship uniqueness applies.
    #[test]
    fn two_hop_count_is_thread_count_invariant() {
        let dir = TempDir::new().unwrap();
        let g = Graph::open(dir.path(), 1).unwrap();
        // A chain, a fan, parallel edges, a self-loop, and a differently labeled
        // node, so every branch of the correction is reachable.
        let ids: Vec<NodeId> = (0..9)
            .map(|i| {
                if i == 8 {
                    g.add_node("Robot", &json!({ "n": i })).unwrap()
                } else {
                    g.add_node("Person", &json!({ "n": i })).unwrap()
                }
            })
            .collect();
        for &(a, b) in &[
            (0, 1),
            (1, 2),
            (2, 3),
            (0, 2),
            (0, 2),
            (3, 3),
            (3, 4),
            (4, 5),
            (5, 6),
            (6, 7),
            (7, 8),
            (8, 0),
        ] {
            g.add_edge(ids[a], ids[b], "F", &json!({})).unwrap();
        }
        g.add_edge(ids[1], ids[2], "B", &json!({})).unwrap();
        g.rebuild_csr().unwrap();

        let specs = [
            PathCountSpec {
                rel_types: vec![Some("F"), Some("F")],
                labels: vec![None, None, None],
                vertex_allow: Vec::new(),
            },
            PathCountSpec {
                rel_types: vec![Some("F"), Some("F")],
                labels: vec![Some("Person"); 3],
                vertex_allow: Vec::new(),
            },
            // Mixed types, so the self-loop correction sees only one of them.
            PathCountSpec {
                rel_types: vec![Some("B"), Some("F")],
                labels: vec![None, None, None],
                vertex_allow: Vec::new(),
            },
            PathCountSpec {
                rel_types: vec![None, None],
                labels: vec![None, Some("Person"), None],
                vertex_allow: Vec::new(),
            },
        ];

        for spec in &specs {
            FORCE_KERNEL_THREADS.with(|f| f.set(0));
            let serial = g.count_linear_paths(spec).unwrap();
            for threads in [2usize, 3, 5, 16] {
                FORCE_KERNEL_THREADS.with(|f| f.set(threads));
                let parallel = g.count_linear_paths(spec).unwrap();
                assert_eq!(
                    serial, parallel,
                    "two-hop count changed at {threads} threads (serial {serial})"
                );
            }
            FORCE_KERNEL_THREADS.with(|f| f.set(0));
            assert!(
                serial > 0,
                "fixture must produce paths for a meaningful check"
            );
        }
    }
}
