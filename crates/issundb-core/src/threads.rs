//! One resolution of the thread budget, shared by every parallel consumer.
//!
//! Two things run in parallel inside the engine: the GraphBLAS OpenMP pool that
//! backs the matrix algorithms, and the scoped-thread reductions in the counting
//! kernels. Both are configured by the same knob ([`crate::Graph::set_thread_count`]
//! and `ISSUNDB_NUM_THREADS`), so both must resolve it the same way. Resolving it
//! twice let the same value mean two different things: with nothing configured the
//! matrices took one thread while a kernel pass took the whole machine, and the
//! two pools could oversubscribe each other.

/// Upper bound on threads any single pass will use, so a misconfigured value
/// cannot spawn an unbounded pool.
pub(crate) const MAX_THREADS: usize = 64;

/// Resolve the thread count for a parallel pass.
///
/// Precedence, first positive value wins:
///
/// 1. `programmatic` — the value [`crate::Graph::set_thread_count`] stored, when
///    positive. Zero means "unset", which is what that method documents.
/// 2. `ISSUNDB_NUM_THREADS` — this engine's own environment override.
/// 3. `OMP_NUM_THREADS` — the ecosystem-standard cap. Honored because GraphBLAS's
///    pool is an OpenMP pool, and because setting it is how a caller (including
///    this repository's own coverage job) caps that pool; resolving it here keeps
///    an explicit `set_global_threads` call from overriding a cap set deliberately.
/// 4. The machine's available parallelism.
///
/// The result is clamped to `1..=MAX_THREADS`, so a caller never has to handle a
/// zero or absurd count.
pub(crate) fn resolve(programmatic: i32) -> usize {
    let machine = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    resolve_from(
        programmatic,
        std::env::var("ISSUNDB_NUM_THREADS").ok().as_deref(),
        std::env::var("OMP_NUM_THREADS").ok().as_deref(),
        machine,
    )
}

/// [`resolve`] with its inputs supplied, so the precedence is testable without
/// mutating process-global environment variables (which would race across the
/// test binary's threads).
pub(crate) fn resolve_from(
    programmatic: i32,
    issundb_env: Option<&str>,
    omp_env: Option<&str>,
    machine: usize,
) -> usize {
    if programmatic > 0 {
        return (programmatic as usize).clamp(1, MAX_THREADS);
    }
    for value in [issundb_env, omp_env].into_iter().flatten() {
        // A malformed or non-positive setting is treated as unset rather than as
        // an error: a thread count is a performance hint, and failing a query
        // over a typo in an environment variable would be worse than ignoring it.
        if let Some(n) = value.trim().parse::<usize>().ok().filter(|n| *n > 0) {
            return n.clamp(1, MAX_THREADS);
        }
    }
    machine.clamp(1, MAX_THREADS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The programmatic override wins over both environment variables, and zero
    /// or negative means unset rather than "no threads".
    #[test]
    fn programmatic_override_takes_precedence() {
        assert_eq!(resolve_from(4, Some("2"), Some("1"), 16), 4);
        assert_eq!(resolve_from(1, Some("8"), None, 16), 1);
        // Unset falls through to the environment.
        assert_eq!(resolve_from(0, Some("2"), Some("1"), 16), 2);
        assert_eq!(resolve_from(-3, Some("2"), None, 16), 2);
    }

    /// `ISSUNDB_NUM_THREADS` outranks `OMP_NUM_THREADS`, which outranks the
    /// machine; with nothing set the whole machine is used.
    #[test]
    fn environment_order_then_machine() {
        assert_eq!(resolve_from(0, Some("3"), Some("7"), 16), 3);
        assert_eq!(resolve_from(0, None, Some("7"), 16), 7);
        assert_eq!(resolve_from(0, None, None, 16), 16);
        // An OpenMP cap of one applies to every pool, which is what the coverage
        // job relies on to keep the pools from oversubscribing.
        assert_eq!(resolve_from(0, None, Some("1"), 16), 1);
    }

    /// A malformed or non-positive environment value is ignored, not fatal, and
    /// the next source is consulted.
    #[test]
    fn malformed_environment_values_fall_through() {
        assert_eq!(resolve_from(0, Some("not-a-number"), Some("2"), 16), 2);
        assert_eq!(resolve_from(0, Some("0"), Some("2"), 16), 2);
        assert_eq!(resolve_from(0, Some(""), None, 16), 16);
        assert_eq!(resolve_from(0, Some(" 5 "), None, 16), 5);
    }

    /// Every path is clamped, so no caller sees zero or an unbounded count.
    #[test]
    fn results_are_always_clamped() {
        assert_eq!(resolve_from(10_000, None, None, 16), MAX_THREADS);
        assert_eq!(resolve_from(0, Some("10000"), None, 16), MAX_THREADS);
        assert_eq!(resolve_from(0, None, None, 0), 1);
        assert_eq!(resolve_from(0, None, None, usize::MAX), MAX_THREADS);
        // The live resolver agrees with the bounds whatever the environment holds.
        let live = resolve(0);
        assert!((1..=MAX_THREADS).contains(&live));
    }
}
