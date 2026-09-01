pub fn percentile(values: &[u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    let rank = (values.len() * percent)
        .div_ceil(100)
        .clamp(1, values.len());
    values[rank - 1]
}
