//! Small statistics helpers.

/// The median of `values`, or `None` when there are none.
///
/// For an even number of values this is the mean of the two middle ones.
pub fn median(values: &[i64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        Some(sorted[mid] as f64)
    } else {
        Some(sorted[mid] as f64)
    }
}

/// The arithmetic mean of `values`, or `None` when there are none.
pub fn mean(values: &[i64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<i64>() as f64 / values.len() as f64)
}
