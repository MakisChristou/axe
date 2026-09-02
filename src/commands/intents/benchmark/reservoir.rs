use std::sync::{Mutex, PoisonError};

use rand::Rng;

use super::types::Sample;

const RETAINED_SAMPLES: usize = 100_000;

pub(super) struct SampleReservoir {
    capacity: usize,
    state: Mutex<ReservoirState>,
}

struct ReservoirState {
    seen: u64,
    retained: Vec<Sample>,
}

impl SampleReservoir {
    pub fn new() -> Self {
        Self::with_capacity(RETAINED_SAMPLES)
    }

    pub fn record(&self, sample: Sample) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.seen = state.seen.saturating_add(1);
        let capacity = u64::try_from(self.capacity).unwrap_or(u64::MAX);
        let replacement = if state.seen <= capacity {
            usize::try_from(state.seen.saturating_sub(1)).ok()
        } else {
            let candidate = rand::thread_rng().gen_range(0..state.seen);
            (candidate < capacity)
                .then(|| usize::try_from(candidate).ok())
                .flatten()
        };
        let Some(replacement) = replacement else {
            return;
        };
        if replacement == state.retained.len() {
            state.retained.push(sample);
        } else if let Some(slot) = state.retained.get_mut(replacement) {
            *slot = sample;
        }
    }

    pub fn snapshot(&self) -> Vec<Sample> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retained
            .clone()
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(ReservoirState {
                seen: 0,
                retained: Vec::with_capacity(capacity),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::intents::benchmark::types::{Sample, SampleOutcome};

    #[test]
    fn reservoir_never_exceeds_its_capacity() {
        let reservoir = SampleReservoir::with_capacity(3);
        for latency_ms in 1..=100 {
            reservoir.record(Sample {
                latency_ms,
                outcome: SampleOutcome::Unavailable,
            });
        }

        assert_eq!(reservoir.snapshot().len(), 3);
    }
}
