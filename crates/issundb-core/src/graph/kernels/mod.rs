//! Graph algorithm kernels over the in-memory CSR snapshot, split by family.
//!
//! Almost every kernel here reads [`crate::csr::CsrSnapshot`] and nothing else, so
//! one freshness gate ([`Graph::ensure_snapshot_fresh`], reached through
//! [`Graph::with_snapshot`]) covers them. A kernel needing a per-edge property the
//! snapshot does not carry (the weight-property algorithms) reads that one property
//! from storage per call, behind the same gate.
//!
//! `label_propagation_kernel` is the exception, and an unhappy one: it takes no
//! snapshot at all and walks `Graph::all_neighbors` per node per iteration, so it
//! opens two read transactions per node per round and needs no gate because it never
//! reads a cache. It predates this module and was carried over unchanged. Reading the
//! adjacency rows every sibling already has would make it both gated and far cheaper;
//! until then, do not cite it as the pattern for a new kernel.
//!
//! Sequencing is deliberate wherever a result is observable. A traversal reports
//! the nodes it reached in ascending dense-index order, which is ascending node id
//! order because the builder sorts `dense_to_id`, and a level-synchronous search
//! orders each frontier the same way. Betweenness accumulates over sources and
//! predecessors in that order too, so its floating-point total is reproducible run
//! to run rather than merely close.

use super::*;

mod analytics;
mod flow;
mod paths;
pub(crate) mod traversal;
