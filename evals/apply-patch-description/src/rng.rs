//! Deterministic SHA-256 counter-based random generation.

use sha2::{Digest, Sha256};

use crate::frame;

/// A reproducible random byte stream with length-framed domain separation.
#[derive(Clone, Debug)]
pub struct CounterRng {
    seed: [u8; 32],
    counter: u64,
    block: [u8; 32],
    offset: usize,
}

impl CounterRng {
    /// Constructs a stream from a domain and an ordered set of seed fields.
    pub fn new(domain: &[u8], seed_fields: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        frame(&mut hasher, b"aj-apply-patch-eval-rng-v1");
        frame(&mut hasher, domain);
        for field in seed_fields {
            frame(&mut hasher, field);
        }
        Self {
            seed: hasher.finalize().into(),
            counter: 0,
            block: [0; 32],
            offset: 32,
        }
    }

    fn refill(&mut self) {
        let mut hasher = Sha256::new();
        frame(&mut hasher, b"aj-apply-patch-eval-rng-block-v1");
        frame(&mut hasher, &self.seed);
        frame(&mut hasher, &self.counter.to_be_bytes());
        self.block = hasher.finalize().into();
        self.counter = self.counter.checked_add(1).expect("RNG counter exhausted");
        self.offset = 0;
    }

    /// Returns the next uniformly distributed `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        for byte in &mut bytes {
            if self.offset == self.block.len() {
                self.refill();
            }
            *byte = self.block[self.offset];
            self.offset += 1;
        }
        u64::from_be_bytes(bytes)
    }

    /// Selects uniformly from `0..upper` using rejection sampling.
    pub fn bounded(&mut self, upper: u64) -> u64 {
        bounded_with(upper, || self.next_u64())
    }

    /// Returns an unbiased boolean.
    pub fn boolean(&mut self) -> bool {
        self.bounded(2) == 1
    }

    /// Shuffles a slice with Fisher-Yates.
    pub fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            let selected = usize::try_from(
                self.bounded(u64::try_from(index + 1).expect("slice length fits u64")),
            )
            .expect("bounded slice index fits usize");
            values.swap(index, selected);
        }
    }
}

fn bounded_with(upper: u64, mut next: impl FnMut() -> u64) -> u64 {
    assert!(
        upper > 0,
        "bounded selection requires a nonzero upper bound"
    );
    let zone = u64::MAX - u64::MAX % upper;
    loop {
        let value = next();
        if value < zone {
            return value % upper;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_is_deterministic_and_domain_separated() {
        let mut first = CounterRng::new(b"schedule", &[b"seed", b"task"]);
        let mut second = CounterRng::new(b"schedule", &[b"seed", b"task"]);
        assert_eq!(first.next_u64(), 10_421_447_059_620_305_580);
        assert_eq!(second.next_u64(), 10_421_447_059_620_305_580);
        assert_eq!(first.next_u64(), second.next_u64());

        let mut other = CounterRng::new(b"fixture", &[b"seed", b"task"]);
        assert_ne!(first.next_u64(), other.next_u64());
    }

    #[test]
    fn framing_distinguishes_field_boundaries() {
        let mut split = CounterRng::new(b"x", &[b"ab", b"c"]);
        let mut joined = CounterRng::new(b"x", &[b"a", b"bc"]);
        assert_ne!(split.next_u64(), joined.next_u64());
    }

    #[test]
    fn bounded_handles_rejection_edge_without_overflow() {
        let mut rng = CounterRng::new(b"bounded", &[b"test"]);
        for upper in [1, 2, 3, u64::from(u32::MAX), u64::MAX] {
            for _ in 0..100 {
                assert!(rng.bounded(upper) < upper);
            }
        }
        let mut values = [u64::MAX, 7].into_iter();
        assert_eq!(bounded_with(3, || values.next().unwrap()), 1);
    }

    #[test]
    fn shuffle_is_deterministic_and_preserves_members() {
        let mut values = (0..16).collect::<Vec<_>>();
        CounterRng::new(b"shuffle", &[b"seed"]).shuffle(&mut values);
        assert_eq!(
            values,
            [9, 5, 4, 1, 3, 8, 15, 11, 13, 7, 6, 10, 12, 14, 2, 0]
        );
        values.sort_unstable();
        assert_eq!(values, (0..16).collect::<Vec<_>>());
    }
}
