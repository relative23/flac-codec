// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Predictions summed exactly in floating point
//!
//! A prediction is a sum of samples times coefficients, in 64-bit integers.
//! Multiplying 64-bit integers does not vectorize; multiplying `f64` does,
//! and gives the same result wherever every value along the way is an
//! integer that `f64` holds exactly, which is any integer of magnitude up
//! to 2^53.
//!
//! A sample of at most 32 bits has a magnitude of at most 2^31. While the
//! magnitudes of the coefficients add up to less than [`COEFFICIENTS`]
//! (2^20), every product, and every sum of products in any order, is an
//! integer of magnitude below 2^51. FLAC's coefficients have at most 15
//! bits and there are at most 32 of them, which stays below that bound,
//! so the check only ever fails for values no stream can hold.

/// The sum of the coefficients' magnitudes below which sums of 32-bit
/// samples times the coefficients are exact in `f64`
pub(crate) const COEFFICIENTS: u64 = 1 << 20;

/// Whether coefficients of these magnitudes keep sums with 32-bit samples
/// exact in `f64`
#[inline]
pub(crate) fn sums_are_exact(magnitudes: impl IntoIterator<Item = u64>) -> bool {
    magnitudes
        .into_iter()
        .try_fold(0u64, |sum, magnitude| sum.checked_add(magnitude))
        .is_some_and(|sum| sum < COEFFICIENTS)
}

/// An integer of magnitude below 2^51 that is held in a `f64`, as an `i64`
///
/// Adding 1.5 × 2^52 moves the value into the range from 2^52 to 2^53, where
/// the bits of a `f64` below its exponent are the number itself less 2^52.
/// The difference of the two bit patterns is therefore the integer. This
/// takes an addition and a subtraction and vectorizes, where the ordinary
/// conversion checks for values that cannot occur here and does not.
#[inline]
pub(crate) fn integer(value: f64) -> i64 {
    const OFFSET: f64 = 6_755_399_441_055_744.0;

    debug_assert!(value.abs() < 2_251_799_813_685_248.0 && value.fract() == 0.0);
    ((value + OFFSET).to_bits() as i64).wrapping_sub(OFFSET.to_bits() as i64)
}

#[test]
fn integer_is_the_integer() {
    assert_eq!(1.5 * 2f64.powi(52), 6_755_399_441_055_744.0);
    let limit = (1i64 << 51) - 1;
    for value in [
        0,
        1,
        -1,
        2,
        -2,
        255,
        -256,
        limit,
        -limit,
        limit - 1,
        1 - limit,
    ] {
        assert_eq!(integer(value as f64), value);
    }
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..100_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let value = (seed as i64) >> (13 + seed % 51);
        assert_eq!(integer(value as f64), value);
    }
}

#[test]
fn the_bound_keeps_every_sum_below_2_to_the_51() {
    // the largest sample magnitude times the largest allowed sum of
    // coefficient magnitudes
    let largest = (1u64 << 31) * (COEFFICIENTS - 1);
    assert!(largest < 1 << 51);
    assert_eq!(integer(largest as f64), largest as i64);
    assert_eq!(integer(-(largest as f64)), -(largest as i64));

    // 32 coefficients of 15 bits, the most a stream can hold, are within it
    assert!(sums_are_exact([1 << 14; 32]));
    assert!(sums_are_exact([COEFFICIENTS - 1]));
    assert!(!sums_are_exact([COEFFICIENTS]));
    assert!(!sums_are_exact([COEFFICIENTS - 1, 1]));
    // magnitudes that overflow their own sum are not within it either
    assert!(!sums_are_exact([u64::MAX, u64::MAX]));
    assert!(sums_are_exact([]));
}
