//! The random numbers of a run. The same seed gives the same numbers.

use std::time::Duration;

/// SplitMix64: a generator with a state of 64 bits.
#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed ^ 0x5DEE_CE66_D1CE_4E5B)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Return a number below `n`, or 0 when `n` is 0.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Return true with the probability `p`.
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        unit < p
    }

    /// Return a duration from `low` to `high`, both included.
    pub(crate) fn between(&mut self, low: Duration, high: Duration) -> Duration {
        let (low, high) = (low.as_nanos() as u64, high.as_nanos() as u64);
        Duration::from_nanos(low + self.below(high.saturating_sub(low) + 1))
    }

    /// Return a new generator for an actor. Its numbers do not depend on
    /// the numbers that the others draw later.
    pub(crate) fn fork(&mut self) -> Rng {
        Rng(self.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_gives_the_same_numbers() {
        let mut first = Rng::new(7);
        let mut second = Rng::new(7);
        let numbers: Vec<u64> = (0..8).map(|_| first.next_u64()).collect();
        assert_eq!(
            numbers,
            (0..8).map(|_| second.next_u64()).collect::<Vec<_>>()
        );
        assert_ne!(Rng::new(8).next_u64(), numbers[0]);
        let low = Duration::from_millis(1);
        let high = Duration::from_millis(3);
        for _ in 0..100 {
            let between = first.between(low, high);
            assert!(low <= between && between <= high);
            assert!(first.below(5) < 5);
        }
        assert_eq!(first.below(0), 0);
    }
}
