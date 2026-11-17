//! The PCG64 sequence and distributions used by the Python QA harness.

use std::collections::HashSet;

pub(super) struct Pcg64 {
    state: u128,
    increment: u128,
    upper: Option<u32>,
}

impl Pcg64 {
    pub(super) fn for_seed_zero() -> Self {
        // NumPy SeedSequence(0), exposed by PCG64(0).state.
        Self::new(
            0x1aa1_b534_5996_452d_0958_5eb7_a695_61e3,
            0x418d_dadb_3af7_1a82_5881_33bc_4478_73a9,
        )
    }

    pub(super) const fn new(state: u128, increment: u128) -> Self {
        Self {
            state,
            increment,
            upper: None,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645)
            .wrapping_add(self.increment);
        let word = ((self.state >> 64) ^ self.state) as u64;
        word.rotate_right((self.state >> 122) as u32)
    }

    pub(super) fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn next_u32(&mut self) -> u32 {
        if let Some(high) = self.upper.take() {
            return high;
        }
        let word = self.next_u64();
        self.upper = Some((word >> 32) as u32);
        word as u32
    }

    fn bounded_index(&mut self, max: usize) -> usize {
        if max == 0 {
            return 0;
        }
        // Cube sizes fit in 32 bits. NumPy uses 32-bit multiply/reject
        // for small bounds and keeps the unused half of each PCG64 word.
        let range = max as u32 + 1;
        let reject_below = range.wrapping_neg() % range;
        loop {
            let product = u64::from(self.next_u32()) * u64::from(range);
            if product as u32 >= reject_below {
                return (product >> 32) as usize;
            }
        }
    }

    /// The set selected by NumPy's choice(replace=False) from a cube grid.
    pub(super) fn choose(&mut self, population: usize, count: usize) -> Vec<usize> {
        if count == population {
            // A convex hull is invariant under shuffling all points.
            return (0..population).collect();
        }
        if population > 10_000 && count > population / 50 {
            // Partial Fisher–Yates shuffle. Sparse swaps capture the tail
            // without allocating the whole permutation.
            let mut swaps = std::collections::HashMap::with_capacity(count);
            let mut result = Vec::with_capacity(count);
            for i in (population - count..population).rev() {
                let j = self.bounded_index(i);
                let at_i = swaps.remove(&i).unwrap_or(i);
                let at_j = swaps.insert(j, at_i).unwrap_or(j);
                result.push(if i == j { at_i } else { at_j });
            }
            return result;
        }
        // Floyd selection. The final shuffle changes the order only.
        let mut seen = HashSet::with_capacity(count);
        let mut result = Vec::with_capacity(count);
        for j in population - count..population {
            let candidate = self.bounded_index(j);
            let value = if seen.insert(candidate) {
                candidate
            } else {
                seen.insert(j);
                j
            };
            result.push(value);
        }
        result
    }
}
const NORMAL_R: f64 = 3.654_152_885_361_009;
const NORMAL_AREA: f64 = 0.004_928_673_233_974_655;
const MAGNITUDE_COUNT: f64 = (1_u64 << 52) as f64;

pub(super) struct Normal {
    widths: [f64; 256],
    densities: [f64; 256],
    thresholds: [u64; 256],
}

impl Normal {
    pub(super) fn new() -> Self {
        // Marsaglia–Tsang's 256 equal-area rectangles. Reconstruct the
        // tables from the area and tail cutoff used by NumPy rather than
        // keeping three large constant tables in the diagnostic.
        let density = |x: f64| (-0.5 * x * x).exp();
        let mut widths = [0.0; 256];
        let mut densities = [0.0; 256];
        let mut thresholds = [0; 256];
        let mut x = NORMAL_R;
        widths[255] = x;
        densities[255] = density(x);
        widths[0] = NORMAL_AREA / densities[255];
        densities[0] = 1.0;
        thresholds[0] = (MAGNITUDE_COUNT * (x / widths[0])) as u64;
        for i in (1..255).rev() {
            let inner = (-2.0 * (NORMAL_AREA / x + density(x)).ln()).sqrt();
            widths[i] = inner;
            densities[i] = density(inner);
            thresholds[i + 1] = (MAGNITUDE_COUNT * (inner / x)) as u64;
            x = inner;
        }
        Self {
            widths: widths.map(|x| x / MAGNITUDE_COUNT),
            densities,
            thresholds,
        }
    }

    pub(super) fn sample(&self, rng: &mut Pcg64) -> f64 {
        loop {
            let word = rng.next_u64();
            let i = (word & 255) as usize;
            let mag = (word >> 9) & ((1_u64 << 52) - 1);
            let sign = if word & 256 == 0 { 1.0 } else { -1.0 };
            let x = sign * mag as f64 * self.widths[i];
            if mag < self.thresholds[i] {
                return x;
            }
            if i == 0 {
                loop {
                    let tail = -(-rng.uniform()).ln_1p() / NORMAL_R;
                    let height = -(-rng.uniform()).ln_1p();
                    if 2.0 * height > tail * tail {
                        let sign = if (mag >> 8) & 1 == 0 { 1.0 } else { -1.0 };
                        return sign * (NORMAL_R + tail);
                    }
                }
            }
            let y = self.densities[i] + (self.densities[i - 1] - self.densities[i]) * rng.uniform();
            if y < (-0.5 * x * x).exp() {
                return x;
            }
        }
    }
}
