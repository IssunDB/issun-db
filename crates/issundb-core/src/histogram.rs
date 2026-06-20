//! Equi-depth histograms over property column values, used for filter
//! selectivity estimates. Built lazily from the in-memory property columns;
//! nothing here is persisted on disk.

use serde_json::Value;
use std::cmp::Ordering;

/// One slice of the value distribution: a closed range with its row count.
#[derive(Debug, Clone)]
pub(crate) struct HistogramBucket {
    /// Lower bound (inclusive).
    pub(crate) lower: Value,
    /// Upper bound (inclusive).
    pub(crate) upper: Value,
    /// Number of distinct values in this bucket.
    pub(crate) distinct_count: u64,
    /// Number of values in this bucket.
    pub(crate) row_count: u64,
}

impl HistogramBucket {
    fn contains(&self, value: &Value) -> bool {
        // A value incomparable with the bounds (a different scalar kind) is
        // outside every bucket, not inside all of them.
        matches!(
            compare_values(value, &self.lower),
            Some(Ordering::Equal | Ordering::Greater)
        ) && matches!(
            compare_values(value, &self.upper),
            Some(Ordering::Equal | Ordering::Less)
        )
    }
}

/// Divides a column's value range into buckets of roughly equal row counts.
#[derive(Debug, Clone)]
pub(crate) struct Histogram {
    pub(crate) buckets: Vec<HistogramBucket>,
    pub(crate) total_rows: u64,
}

impl Histogram {
    /// Build an equi-depth histogram from values pre-sorted by
    /// [`compare_values`]. Values must be mutually comparable (one scalar
    /// kind); the caller guarantees this by only building over typed columns.
    pub(crate) fn build(sorted_values: &[Value], num_buckets: usize) -> Self {
        let mut buckets = Vec::new();
        if !sorted_values.is_empty() {
            let num_buckets = num_buckets.clamp(1, sorted_values.len());
            let rows_per_bucket = sorted_values.len() / num_buckets;
            let mut start = 0;
            for i in 0..num_buckets {
                let end = if i == num_buckets - 1 {
                    sorted_values.len()
                } else {
                    start + rows_per_bucket
                };
                let bucket_values = &sorted_values[start..end];
                if bucket_values.is_empty() {
                    continue;
                }
                let distinct = 1 + bucket_values.windows(2).filter(|w| w[0] != w[1]).count() as u64;
                buckets.push(HistogramBucket {
                    lower: bucket_values[0].clone(),
                    upper: bucket_values[bucket_values.len() - 1].clone(),
                    distinct_count: distinct,
                    row_count: bucket_values.len() as u64,
                });
                start = end;
            }
        }
        let total_rows = buckets.iter().map(|b| b.row_count).sum();
        Self {
            buckets,
            total_rows,
        }
    }

    /// Estimate the fraction of rows matching an equality predicate, assuming
    /// a uniform distribution over the distinct values inside a bucket.
    pub(crate) fn estimate_equality_selectivity(&self, value: &Value) -> f64 {
        if self.total_rows == 0 {
            return 0.0;
        }
        for bucket in &self.buckets {
            if bucket.contains(value) {
                if bucket.distinct_count == 0 {
                    return 0.0;
                }
                return (bucket.row_count as f64 / bucket.distinct_count as f64)
                    / self.total_rows as f64;
            }
        }
        0.0
    }

    /// Estimate the fraction of rows inside `[lower, upper]` (either bound
    /// optional). Bound inclusivity is not modeled: under the continuous
    /// interpolation inside a bucket, a single boundary value carries no
    /// estimable mass.
    pub(crate) fn estimate_range_selectivity(
        &self,
        lower: Option<&Value>,
        upper: Option<&Value>,
    ) -> f64 {
        if self.total_rows == 0 {
            return 0.0;
        }
        let mut matching_rows = 0.0;
        for bucket in &self.buckets {
            let overlaps = bound_le(lower, &bucket.upper) && bound_ge(upper, &bucket.lower);
            if !overlaps {
                continue;
            }
            let fraction = estimate_bucket_overlap(&bucket.lower, &bucket.upper, lower, upper);
            matching_rows += bucket.row_count as f64 * fraction;
        }
        matching_rows / self.total_rows as f64
    }
}

/// `bound <= value`, where a missing bound is unbounded and an incomparable
/// bound excludes the bucket.
fn bound_le(bound: Option<&Value>, value: &Value) -> bool {
    match bound {
        None => true,
        Some(b) => matches!(
            compare_values(b, value),
            Some(Ordering::Less | Ordering::Equal)
        ),
    }
}

/// `bound >= value`, with the same missing and incomparable semantics.
fn bound_ge(bound: Option<&Value>, value: &Value) -> bool {
    match bound {
        None => true,
        Some(b) => matches!(
            compare_values(b, value),
            Some(Ordering::Greater | Ordering::Equal)
        ),
    }
}

/// Estimate the fraction of a bucket that overlaps with a range, by linear
/// interpolation for numeric bounds and a fixed 0.5 otherwise.
fn estimate_bucket_overlap(
    bucket_lower: &Value,
    bucket_upper: &Value,
    range_lower: Option<&Value>,
    range_upper: Option<&Value>,
) -> f64 {
    let covers_lower = bound_le(range_lower, bucket_lower) || range_lower.is_none();
    let covers_upper = bound_ge(range_upper, bucket_upper) || range_upper.is_none();
    if covers_lower && covers_upper {
        return 1.0;
    }
    match (bucket_lower, bucket_upper) {
        (Value::Number(bl), Value::Number(bu)) => {
            let bl_f = bl.as_f64().unwrap_or(0.0);
            let bu_f = bu.as_f64().unwrap_or(0.0);
            let bucket_range = bu_f - bl_f;
            if bucket_range <= 0.0 {
                return 1.0;
            }
            let effective_lower = range_lower.and_then(|l| l.as_f64()).unwrap_or(bl_f);
            let effective_upper = range_upper.and_then(|u| u.as_f64()).unwrap_or(bu_f);
            let overlap_lower = effective_lower.max(bl_f);
            let overlap_upper = effective_upper.min(bu_f);
            if overlap_upper < overlap_lower {
                return 0.0;
            }
            (overlap_upper - overlap_lower) / bucket_range
        }
        _ => 0.5,
    }
}

/// Compare two JSON scalars of the same kind; `None` for incomparable kinds.
pub(crate) fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(n1), Value::Number(n2)) => {
            if let (Some(i1), Some(i2)) = (n1.as_i64(), n2.as_i64()) {
                Some(i1.cmp(&i2))
            } else if let (Some(f1), Some(f2)) = (n1.as_f64(), n2.as_f64()) {
                f1.partial_cmp(&f2)
            } else {
                None
            }
        }
        (Value::String(s1), Value::String(s2)) => Some(s1.cmp(s2)),
        (Value::Bool(b1), Value::Bool(b2)) => Some(b1.cmp(b2)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn int_values(range: std::ops::Range<i64>) -> Vec<Value> {
        range.map(|i| json!(i)).collect()
    }

    #[test]
    fn build_splits_rows_evenly() {
        let h = Histogram::build(&int_values(0..100), 10);
        assert_eq!(h.buckets.len(), 10);
        assert_eq!(h.total_rows, 100);
        for b in &h.buckets {
            assert_eq!(b.row_count, 10);
            assert_eq!(b.distinct_count, 10);
        }
        assert_eq!(h.buckets[0].lower, json!(0));
        assert_eq!(h.buckets[9].upper, json!(99));
    }

    #[test]
    fn build_caps_buckets_at_value_count() {
        let h = Histogram::build(&int_values(0..3), 10);
        assert_eq!(h.buckets.len(), 3);
        assert_eq!(h.total_rows, 3);
    }

    #[test]
    fn build_empty_is_empty() {
        let h = Histogram::build(&[], 10);
        assert!(h.buckets.is_empty());
        assert_eq!(h.total_rows, 0);
        assert_eq!(h.estimate_equality_selectivity(&json!(1)), 0.0);
        assert_eq!(h.estimate_range_selectivity(None, None), 0.0);
    }

    #[test]
    fn equality_selectivity_uniform_distinct() {
        // 100 distinct values: each carries 1/100 of the rows.
        let h = Histogram::build(&int_values(0..100), 10);
        let sel = h.estimate_equality_selectivity(&json!(42));
        assert!((sel - 0.01).abs() < 1e-9, "got {sel}");
    }

    #[test]
    fn equality_selectivity_skewed_value() {
        // 90 copies of 1 plus 1..=10: the heavy bucket reports its share.
        let mut vals: Vec<Value> = std::iter::repeat_n(json!(1), 90).collect();
        vals.extend((2..=11).map(|i| json!(i)));
        let h = Histogram::build(&vals, 10);
        let sel = h.estimate_equality_selectivity(&json!(1));
        // The first nine buckets are all 1s (10 rows, 1 distinct each), so
        // the point estimate from any of them is 10/100.
        assert!((sel - 0.1).abs() < 1e-9, "got {sel}");
    }

    #[test]
    fn equality_selectivity_out_of_bounds_or_incomparable_is_zero() {
        let h = Histogram::build(&int_values(0..100), 10);
        assert_eq!(h.estimate_equality_selectivity(&json!(1000)), 0.0);
        assert_eq!(h.estimate_equality_selectivity(&json!(-1)), 0.0);
        assert_eq!(h.estimate_equality_selectivity(&json!("zap")), 0.0);
        assert_eq!(h.estimate_equality_selectivity(&Value::Null), 0.0);
    }

    #[test]
    fn range_selectivity_interpolates() {
        let h = Histogram::build(&int_values(0..100), 10);
        let half = h.estimate_range_selectivity(Some(&json!(50)), None);
        assert!((half - 0.5).abs() < 0.05, "got {half}");
        let all = h.estimate_range_selectivity(None, None);
        assert!((all - 1.0).abs() < 1e-9, "got {all}");
        let none = h.estimate_range_selectivity(Some(&json!(1000)), None);
        assert_eq!(none, 0.0);
        let slice = h.estimate_range_selectivity(Some(&json!(20)), Some(&json!(30)));
        assert!((slice - 0.1).abs() < 0.05, "got {slice}");
    }

    #[test]
    fn range_selectivity_incomparable_bound_matches_nothing() {
        let h = Histogram::build(&int_values(0..100), 10);
        assert_eq!(h.estimate_range_selectivity(Some(&json!("a")), None), 0.0);
    }

    #[test]
    fn string_buckets_use_half_bucket_overlap() {
        let vals: Vec<Value> = ["a", "b", "c", "d"].iter().map(|s| json!(s)).collect();
        let h = Histogram::build(&vals, 2);
        let sel = h.estimate_range_selectivity(Some(&json!("b")), None);
        // First bucket [a, b] partially covered (0.5), second [c, d] fully.
        assert!((sel - 0.75).abs() < 1e-9, "got {sel}");
    }

    #[test]
    fn compare_values_crosses_int_and_float() {
        assert_eq!(compare_values(&json!(1), &json!(1.5)), Some(Ordering::Less));
        assert_eq!(
            compare_values(&json!(2.0), &json!(2.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_values(&json!(1), &json!("1")), None);
        assert_eq!(compare_values(&Value::Null, &json!(1)), None);
    }
}
