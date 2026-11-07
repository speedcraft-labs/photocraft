//! In-place order statistics for reusable sample buffers.

/// Returns the upper median, or `None` for an empty slice, rearranging the samples in place.
/// Even lengths use the element at `len / 2`, not the average of the middle pair.
/// Values, including signed zero and NaNs (with their signs and payloads), follow `f32::total_cmp`.
pub fn median_f32(samples: &mut [f32]) -> Option<f32> {
    if samples.is_empty() {
        return None;
    }
    let middle = samples.len() / 2;
    Some(*samples.select_nth_unstable_by(middle, f32::total_cmp).1)
}

/// Returns the upper median under `f64::total_cmp`, or `None` for an empty slice.
/// Like [`median_f32`], this preserves the ordering of signed zeros and all NaN bit patterns.
pub fn median_f64(samples: &mut [f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let middle = samples.len() / 2;
    Some(*samples.select_nth_unstable_by(middle, f64::total_cmp).1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn arbitrary_f32_samples() -> impl Strategy<Value = Vec<f32>> {
        prop::collection::vec(
            prop_oneof![
                any::<u32>().prop_map(f32::from_bits),
                any::<i16>().prop_map(|value| value as f32),
                Just(0.0f32),
                Just(-0.0f32),
                Just(f32::INFINITY),
                Just(f32::NEG_INFINITY),
                Just(f32::NAN),
            ],
            0..1024,
        )
    }

    fn arbitrary_f64_samples() -> impl Strategy<Value = Vec<f64>> {
        prop::collection::vec(
            prop_oneof![
                any::<u64>().prop_map(f64::from_bits),
                any::<i16>().prop_map(|value| value as f64),
                Just(0.0f64),
                Just(-0.0f64),
                Just(f64::INFINITY),
                Just(f64::NEG_INFINITY),
                Just(f64::NAN),
            ],
            0..1024,
        )
    }

    #[test]
    fn empty_samples_have_no_median() {
        let mut f32_samples: [f32; 0] = [];
        let mut f64_samples: [f64; 0] = [];
        assert_eq!(median_f32(&mut f32_samples), None);
        assert_eq!(median_f64(&mut f64_samples), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        #[test]
        fn f32_selection_matches_full_total_order_sort(mut samples in arbitrary_f32_samples()) {
            let mut sorted = samples.clone();
            sorted.sort_by(f32::total_cmp);
            let expected = sorted.get(sorted.len() / 2).map(|value| value.to_bits());
            let actual = median_f32(&mut samples).map(f32::to_bits);
            prop_assert_eq!(actual, expected);
        }

        #[test]
        fn f64_selection_matches_full_total_order_sort(mut samples in arbitrary_f64_samples()) {
            let mut sorted = samples.clone();
            sorted.sort_by(f64::total_cmp);
            let expected = sorted.get(sorted.len() / 2).map(|value| value.to_bits());
            let actual = median_f64(&mut samples).map(f64::to_bits);
            prop_assert_eq!(actual, expected);
        }
    }
}
