//! The execution-strategy switch: answer every query with the general row
//! pipeline instead of a shape-specific fast path.
//!
//! Four different pieces of code can answer the same query. The row pipeline is
//! the general one; the columnar executor in [`crate::exec`], the counting
//! kernels in `issundb-core`, and the fused closing hop each answer a recognized
//! shape without going through it. Every one of them has to reproduce Cypher
//! MATCH semantics independently, including relationship uniqueness, three-valued
//! logic, and the difference between `count(*)` and `count(x.prop)`. Nothing in
//! the type system makes them agree.
//!
//! This switch makes the disagreement testable. With it on, a query that would
//! have taken a fast path is planned and executed the general way, so the two
//! answers can be compared directly, and the row pipeline acts as the oracle.
//! Turn it on for a whole process with `ISSUNDB_ROW_PIPELINE_ONLY=1` to sweep an
//! entire test suite, or for one thread with [`RowPipelineOnly`] inside a test.
//! It is also a support tool: flipping it is the quickest way to establish
//! whether a fast path is responsible for a wrong answer.
//!
//! What it takes out of the plan:
//!
//! - the columnar executor (`exec::vectorized`), which the read path then never
//!   consults,
//! - the `TriangleCount`, `PathCount`, and `GroupedDegree` counting kernels, plus
//!   the count window pushed into the last of those,
//! - the fused `ExpandIntersect` closing hop,
//! - the metadata shortcut for a count over a bare label scan, so the count comes
//!   from a scan rather than from the stored per-label counter, and
//! - the type-inference pruning pass (`prune_unsatisfiable`). This one is not a
//!   faster way to compute the same rows: it drops an `Expand` outright on the word
//!   of the cached data schema, so a wrong `schema_has_edge` negative turns a real
//!   result set into zero rows. Leaving it outside the switch made that invisible,
//!   because both halves of every differential comparison pruned identically and
//!   agreed on nothing.
//!
//! What it deliberately leaves alone:
//!
//! - `VectorTopK`, because an HNSW search is approximate by construction, so the
//!   exact row-pipeline sort it replaces is entitled to return different rows;
//!   comparing the two would test the index's recall, not anyone's semantics, and
//! - the index-scan, correlated-seek, join-to-expand, and `MultiwayJoin`
//!   rewrites, which change which general operators run rather than which
//!   executor evaluates them. The row pipeline answers those plans either way.
//!
//! The flag is per thread, so tests that force it do not disturb the other tests
//! sharing their process.
//!
//! The read path resolves it once per statement and hands that one value to both
//! the optimizer (`Optimizer::optimize_with_mode`) and its choice of executor, so a
//! read statement cannot plan under one mode and execute under the other. Three
//! other places still read it from the ambient thread through the convenience
//! `Optimizer::optimize`: `explain`, which should show what would run on this
//! thread, and the write path's four `Optimizer::optimize` call sites in
//! `exec::write`, which have not been threaded. Thread them if a write-path
//! executor choice is ever made from the flag. A thread spawned mid-execution does
//! not inherit the flag either, which is why the large-stack dispatch in
//! [`crate::exec`] carries it across explicitly.

use std::cell::Cell;

/// Environment variable that turns the switch on for every thread in the
/// process. Any value other than empty or `0` enables it.
const ENV_VAR: &str = "ISSUNDB_ROW_PIPELINE_ONLY";

fn env_default() -> bool {
    std::env::var_os(ENV_VAR).is_some_and(|v| !v.is_empty() && v != "0")
}

thread_local! {
    static ROW_PIPELINE_ONLY: Cell<bool> = Cell::new(env_default());
}

/// Whether the current thread must answer queries with the row pipeline alone.
pub(crate) fn row_pipeline_only() -> bool {
    ROW_PIPELINE_ONLY.with(|f| f.get())
}

/// Guard that forces the row pipeline for the current thread and restores the
/// previous setting on drop, so a nested installation cannot leak the setting
/// past its own scope.
pub(crate) struct RowPipelineOnly {
    previous: bool,
}

impl RowPipelineOnly {
    /// Force the row pipeline for the current scope. Only tests install this
    /// directly; a process-wide run comes from the environment variable instead.
    #[cfg(test)]
    pub(crate) fn install() -> Self {
        RowPipelineOnly {
            previous: ROW_PIPELINE_ONLY.with(|f| f.replace(true)),
        }
    }

    /// Force the setting to `on` for the current thread, restoring the previous
    /// value on drop. Used to carry the setting onto a thread spawned mid-query,
    /// which starts from the environment default rather than from its parent.
    pub(crate) fn install_as(on: bool) -> Self {
        RowPipelineOnly {
            previous: ROW_PIPELINE_ONLY.with(|f| f.replace(on)),
        }
    }
}

impl Drop for RowPipelineOnly {
    fn drop(&mut self) {
        ROW_PIPELINE_ONLY.with(|f| f.set(self.previous));
    }
}

/// Pin the fast paths back on for the current scope, whatever the environment
/// says.
///
/// Two kinds of test need this. One asserts that a particular operator lowers
/// into the plan, so it has no meaning at all with the switch on. The other is a
/// differential comparison, whose fast half has to actually take the fast path or
/// it compares the row pipeline against itself and passes vacuously. Both pin the
/// setting rather than inherit it, which is what lets the whole suite run under
/// `ISSUNDB_ROW_PIPELINE_ONLY=1` as a sweep without those tests either failing on
/// their own premise or quietly testing nothing.
#[cfg(test)]
pub(crate) fn fast_paths_required() -> RowPipelineOnly {
    RowPipelineOnly::install_as(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard sets the flag for its scope and restores the previous value,
    /// including when guards nest.
    #[test]
    fn guard_restores_the_previous_setting() {
        // Pinned off rather than assumed off, so the test means the same thing
        // when the whole suite is swept with the switch on.
        let _baseline = fast_paths_required();
        assert!(!row_pipeline_only());
        {
            let _outer = RowPipelineOnly::install();
            assert!(row_pipeline_only());
            {
                let _inner = RowPipelineOnly::install();
                assert!(row_pipeline_only());
            }
            assert!(row_pipeline_only(), "the inner guard restored, not cleared");
        }
        assert!(!row_pipeline_only());
    }

    /// `install_as(false)` is how a spawned execution thread inherits a parent
    /// that had the switch off, and it restores just like the forcing form.
    #[test]
    fn install_as_carries_either_setting() {
        let _on = RowPipelineOnly::install_as(true);
        assert!(row_pipeline_only());
        {
            let _off = RowPipelineOnly::install_as(false);
            assert!(!row_pipeline_only());
        }
        assert!(row_pipeline_only());
    }
}
