//! Estimating what audio will cost. Pure: no database, no clock.
//!
//! An estimate is a range, never a single number. The price is per million
//! characters spoken; the range covers how fast the voice speaks and how many
//! audio tokens that takes. It is an assumption, dated, and shown as one.

/// Low and high as a share of the likely cost.
pub const LOW_PERCENT: i64 = 75;
pub const HIGH_PERCENT: i64 = 140;
/// Spoken characters per second of audio, for the duration estimate.
pub const CHARS_PER_SECOND: i64 = 14;

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Range {
    pub low: i64,
    pub likely: i64,
    pub high: i64,
}

fn ceil_div(a: i128, b: i128) -> i64 {
    ((a + b - 1) / b) as i64
}

/// The cost range, in micros, of speaking `chars` characters at `per_million` micros per million.
pub fn estimate(chars: i64, per_million: i64) -> Range {
    let likely = ceil_div(chars as i128 * per_million as i128, 1_000_000);
    Range {
        low: likely * LOW_PERCENT / 100,
        likely,
        high: ceil_div(likely as i128 * HIGH_PERCENT as i128, 100),
    }
}

/// A limit to offer: the high end, rounded up to the next ten cents. Zero when there is nothing to make.
pub fn suggested_limit(high: i64) -> i64 {
    if high <= 0 {
        0
    } else {
        ceil_div(high as i128, 100_000) * 100_000
    }
}

pub fn seconds(chars: i64) -> i64 {
    chars / CHARS_PER_SECOND
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_surrounds_the_likely_cost() {
        let r = estimate(400_000, 16_300_000);
        assert_eq!(r.likely, 6_520_000);
        assert_eq!(r.low, 4_890_000);
        assert_eq!(r.high, 9_128_000);
        assert_eq!(
            estimate(0, 16_300_000),
            Range {
                low: 0,
                likely: 0,
                high: 0
            }
        );
        assert!(
            estimate(1, 16_300_000).high >= estimate(1, 16_300_000).likely,
            "never understated"
        );
    }

    #[test]
    fn the_suggested_limit_covers_the_high_end() {
        assert_eq!(suggested_limit(9_128_000), 9_200_000);
        assert_eq!(suggested_limit(9_200_000), 9_200_000);
        assert_eq!(suggested_limit(1), 100_000);
        assert_eq!(suggested_limit(0), 0);
    }
}
