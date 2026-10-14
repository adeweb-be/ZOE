//! The schedule of a client: which objects each transaction reads, and
//! whether it writes them back.
//!
//! The schedule depends only on the seed, the index of the client and the
//! settings, never on the time. So a `--seed` makes the schedule of each
//! client repeat. A retry runs the same transaction again and takes
//! nothing from the generator.

/// A pseudo-random generator: SplitMix64. It is small, fast and the same
/// on each machine.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Return a generator for the client `client` of a run with the seed
    /// `seed`.
    pub fn new(seed: u64, client: u64) -> Rng {
        let mut rng = Rng(seed ^ client.wrapping_mul(0xA076_1D64_78BD_642F));
        rng.next_u64();
        rng
    }

    /// Return the next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Return a number in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A Zipf law over the ranks `0..n`: the rank `k` has a weight of
/// `1 / (k + 1)^exponent`. The exponent 0 gives a uniform law. A larger
/// exponent gives a smaller hot set.
#[derive(Clone, Debug)]
pub struct Zipf {
    /// The cumulative probability of each rank. The last one is 1.
    cumulative: Vec<f64>,
}

impl Zipf {
    /// Return the law over `n` ranks. `n` is at least 1, and `exponent` is
    /// finite and not negative.
    pub fn new(n: usize, exponent: f64) -> Zipf {
        assert!(n >= 1, "a Zipf law needs one rank at least");
        assert!(exponent.is_finite() && exponent >= 0.0, "a bad exponent");
        let mut total = 0.0;
        let mut cumulative: Vec<f64> = (1..=n)
            .map(|rank| {
                total += (rank as f64).powf(-exponent);
                total
            })
            .collect();
        for value in &mut cumulative {
            *value /= total;
        }
        if let Some(last) = cumulative.last_mut() {
            *last = 1.0;
        }
        Zipf { cumulative }
    }

    /// Return the number of ranks.
    pub fn len(&self) -> usize {
        self.cumulative.len()
    }

    /// Return `false`: a law has one rank at least.
    pub fn is_empty(&self) -> bool {
        self.cumulative.is_empty()
    }

    /// Return a rank.
    pub fn sample(&self, rng: &mut Rng) -> usize {
        let u = rng.next_f64();
        self.cumulative
            .partition_point(|&c| c <= u)
            .min(self.len() - 1)
    }
}

/// One transaction of a schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The ranks of the objects that the transaction reads, all different,
    /// in the order of the reads.
    pub ranks: Vec<usize>,
    /// `true` when the transaction writes back the objects that it reads.
    pub write: bool,
}

/// The most draws for one rank before the schedule takes the next free
/// rank in order. A steep law draws the same hot ranks again and again.
const DRAWS_PER_RANK: usize = 64;

/// The transactions of one client, one after the other.
#[derive(Clone, Debug)]
pub struct Schedule {
    rng: Rng,
    zipf: Zipf,
    size: usize,
    write_ratio: f64,
}

impl Schedule {
    /// Return the schedule of the client `client`. Each transaction reads
    /// `size` different objects of the law `zipf`, and writes them back
    /// with the probability `write_ratio`. `size` is at most the number of
    /// ranks of `zipf`.
    pub fn new(seed: u64, client: u64, zipf: Zipf, size: usize, write_ratio: f64) -> Schedule {
        assert!(size <= zipf.len(), "a transaction larger than the store");
        Schedule {
            rng: Rng::new(seed, client),
            zipf,
            size,
            write_ratio,
        }
    }

    /// Return the next transaction.
    pub fn next_step(&mut self) -> Step {
        let mut ranks = Vec::with_capacity(self.size);
        let mut draws = 0;
        while ranks.len() < self.size {
            let rank = if draws < DRAWS_PER_RANK * self.size {
                draws += 1;
                self.zipf.sample(&mut self.rng)
            } else {
                // The first free rank. The loop ends because size is at
                // most the number of ranks.
                (0..self.zipf.len())
                    .find(|rank| !ranks.contains(rank))
                    .expect("a free rank")
            };
            if !ranks.contains(&rank) {
                ranks.push(rank);
            }
        }
        let write = self.rng.next_f64() < self.write_ratio;
        Step { ranks, write }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_gives_the_same_schedule_again() {
        let steps = |seed| {
            let mut schedule = Schedule::new(seed, 3, Zipf::new(100, 1.2), 5, 0.5);
            (0..50).map(|_| schedule.next_step()).collect::<Vec<_>>()
        };
        assert_eq!(steps(7), steps(7));
        assert_ne!(steps(7), steps(8));
    }

    #[test]
    fn each_client_has_its_own_schedule() {
        let first = Schedule::new(7, 0, Zipf::new(100, 1.0), 5, 0.5).next_step();
        let second = Schedule::new(7, 1, Zipf::new(100, 1.0), 5, 0.5).next_step();
        assert_ne!(first, second);
    }

    #[test]
    fn a_step_reads_different_objects() {
        let mut schedule = Schedule::new(1, 0, Zipf::new(10, 1.2), 10, 0.0);
        for _ in 0..20 {
            let mut ranks = schedule.next_step().ranks;
            ranks.sort_unstable();
            assert_eq!(ranks, (0..10).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_steep_law_still_fills_a_step() {
        let mut schedule = Schedule::new(1, 0, Zipf::new(1000, 40.0), 8, 0.0);
        assert_eq!(schedule.next_step().ranks.len(), 8);
    }

    #[test]
    fn the_write_ratio_sets_the_share_of_writes() {
        let mut schedule = Schedule::new(1, 0, Zipf::new(10, 0.0), 1, 0.25);
        let writes = (0..10_000).filter(|_| schedule.next_step().write).count();
        assert!((2_200..2_800).contains(&writes), "{writes}");
        let mut never = Schedule::new(1, 0, Zipf::new(10, 0.0), 1, 0.0);
        assert!((0..1_000).all(|_| !never.next_step().write));
        let mut always = Schedule::new(1, 0, Zipf::new(10, 0.0), 1, 1.0);
        assert!((0..1_000).all(|_| always.next_step().write));
    }

    #[test]
    fn the_zipf_law_makes_a_hot_set() {
        let zipf = Zipf::new(1000, 1.2);
        let mut rng = Rng::new(1, 0);
        let mut counts = vec![0usize; 1000];
        for _ in 0..100_000 {
            counts[zipf.sample(&mut rng)] += 1;
        }
        // The first rank is the hottest, and the first ten ranks take
        // more than half of the draws.
        assert!(counts[0] > counts[1] && counts[1] > counts[10]);
        assert!(counts[..10].iter().sum::<usize>() > 50_000);
    }

    #[test]
    fn the_exponent_zero_is_uniform() {
        let zipf = Zipf::new(4, 0.0);
        let mut rng = Rng::new(2, 0);
        let mut counts = [0usize; 4];
        for _ in 0..40_000 {
            counts[zipf.sample(&mut rng)] += 1;
        }
        assert!(
            counts.iter().all(|&c| (9_000..11_000).contains(&c)),
            "{counts:?}"
        );
    }

    #[test]
    fn a_number_is_in_the_unit_interval() {
        let mut rng = Rng::new(0, 0);
        assert!(
            (0..10_000)
                .map(|_| rng.next_f64())
                .all(|x| (0.0..1.0).contains(&x))
        );
    }
}
