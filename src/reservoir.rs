pub(crate) struct Reservoir<T: Default> {
    count: usize,
    events: Box<[T]>,
}

impl<T: Default> Reservoir<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        let mut events = Vec::with_capacity(capacity);
        events.resize_with(capacity, T::default);
        Self {
            count: 0,
            events: events.into_boxed_slice(),
        }
    }

    pub(crate) fn sample(&mut self, event: T) -> T {
        self.count += 1;

        if let Some(slot) = self.events.get_mut(self.count - 1) {
            std::mem::replace(slot, event)
        } else if let Some(slot) = self.events.get_mut(fastrand::usize(0..self.count)) {
            std::mem::replace(slot, event)
        } else {
            event
        }
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        let iter = self.events.iter_mut().map(std::mem::take).take(self.count);
        self.count = 0;
        iter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underfull() {
        let mut reservoir = Reservoir::new(10);
        for i in 1..=5 {
            assert!(reservoir.sample(i) == 0);
        }
        assert_eq!(reservoir.count, 5);
        let drained: Vec<_> = reservoir.drain().collect();
        assert_eq!(drained, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn overfull_returns_ejected() {
        let mut reservoir = Reservoir::new(10);
        let mut ejected_count = 0;
        for i in 1..=1000 {
            if reservoir.sample(i) != 0 {
                ejected_count += 1;
            }
        }
        assert_eq!(reservoir.count, 1000);
        assert_eq!(ejected_count, 990);
        let drained: Vec<_> = reservoir.drain().collect();
        assert_eq!(drained.len(), 10);
    }

    /// Chi-squared goodness-of-fit test for reservoir sampling uniformity.
    ///
    /// Runs many trials of sampling N items into a reservoir of size K,
    /// counts how often each item index appears, and checks that the
    /// distribution is uniform via the chi-squared statistic.
    #[test]
    #[ignore]
    fn uniformity_chi_squared() {
        const N: usize = 50;
        const K: usize = 10;
        const TRIALS: usize = 500_000;

        let mut counts = vec![0u64; N];

        for _ in 0..TRIALS {
            let mut reservoir: Reservoir<usize> = Reservoir::new(K);
            for i in 1..=N {
                reservoir.sample(i);
            }
            for item in reservoir.drain() {
                counts[item - 1] += 1;
            }
        }

        let expected = (TRIALS * K) as f64 / N as f64;
        let chi_sq: f64 = counts
            .iter()
            .map(|&c| {
                let diff = c as f64 - expected;
                diff * diff / expected
            })
            .sum();

        let df = (N - 1) as f64;
        let dist = statrs::distribution::ChiSquared::new(df).unwrap();
        let p_value = 1.0 - statrs::distribution::ContinuousCDF::cdf(&dist, chi_sq);

        assert!(
            p_value > 0.001,
            "chi-squared {chi_sq:.1} with df={df}, p-value={p_value:.6} — \
             distribution is not uniform (p < 0.001)"
        );
    }
}
