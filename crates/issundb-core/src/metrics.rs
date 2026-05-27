/// Pluggable observability interface for index internals.
///
/// Implement this trait to collect per-operation statistics from FTS and vector
/// index operations. The default [`NoOpMetrics`] discards all events; replace it
/// with a real collector to expose counts to Prometheus, tracing, etc.
///
/// All methods have default no-op implementations so implementors only need to
/// override the events they care about.
pub trait MetricsCollector: Send + Sync + 'static {
    /// A key comparison was performed (e.g., a key lookup or cursor step in LMDB).
    fn record_comparison(&self) {}
    /// An index page or LMDB data item was loaded from storage.
    fn record_index_load(&self) {}
    /// One entry from a posting list was examined during FTS scoring.
    fn record_posting_visited(&self) {}
    /// A candidate document was pruned by the WAND threshold without full scoring.
    fn record_wand_prune(&self) {}
    /// A full BM25 or TF-IDF score was computed for a candidate document.
    fn record_score_computed(&self) {}
}

/// A no-op [`MetricsCollector`] that discards all events.
///
/// This is the zero-cost default used when the caller does not supply a
/// custom collector. All methods are empty and will be elided by the compiler.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpMetrics;

impl MetricsCollector for NoOpMetrics {}
