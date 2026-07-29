use std::num::NonZeroU64;

use eyre::{Result, eyre};

use super::LoadTestArgs;

/// A validated load-test run mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunMode {
    Burst { num_txs: u64 },
    Sustained(SustainedPlan),
}

/// A validated sustained-mode schedule. Named fields keep call sites from
/// destructuring a positional tuple whose three numbers all look alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SustainedPlan {
    pub tps: usize,
    pub duration_secs: u64,
    pub key_cycle: usize,
}

impl SustainedPlan {
    /// Total transactions the schedule will fire. Overflow-free: `RunSizing`
    /// rejects any `tps * duration_secs` that does not fit in a `u64`.
    pub fn total_transactions(self) -> u64 {
        (self.tps as u64).saturating_mul(self.duration_secs)
    }
}

/// Validated run mode plus the derived wallet and transaction counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RunSizing {
    mode: RunMode,
    pub num_keys: usize,
    pub total_expected: u64,
}

impl RunSizing {
    pub fn new(args: &LoadTestArgs) -> Result<Self> {
        Self::from_values(args.num_txs, args.tps, args.duration_secs, args.key_cycle)
    }

    fn from_values(
        num_txs: u64,
        tps: Option<u64>,
        duration_secs: Option<u64>,
        key_cycle: u64,
    ) -> Result<Self> {
        match (tps, duration_secs) {
            (None, None) => {
                let num_txs = nonzero(num_txs, "--num-txs")?.get();
                let num_keys = usize::try_from(num_txs)
                    .map_err(|_| eyre!("--num-txs exceeds the supported wallet count"))?;
                Ok(Self {
                    mode: RunMode::Burst { num_txs },
                    num_keys,
                    total_expected: num_txs,
                })
            }
            (Some(tps), Some(duration_secs)) => {
                let tps = nonzero(tps, "--tps")?.get();
                let duration_secs = nonzero(duration_secs, "--duration-secs")?.get();
                let key_cycle = nonzero(key_cycle, "--key-cycle")?.get();

                let total_expected = tps.checked_mul(duration_secs).ok_or_else(|| {
                    eyre!(
                        "--tps multiplied by --duration-secs exceeds the supported transaction count"
                    )
                })?;
                let tps = usize::try_from(tps)
                    .map_err(|_| eyre!("--tps exceeds the supported per-second rate"))?;
                let key_cycle = usize::try_from(key_cycle)
                    .map_err(|_| eyre!("--key-cycle exceeds the supported interval"))?;
                let num_keys = tps.checked_mul(key_cycle).ok_or_else(|| {
                    eyre!("--tps multiplied by --key-cycle exceeds the supported wallet count")
                })?;

                Ok(Self {
                    mode: RunMode::Sustained(SustainedPlan {
                        tps,
                        duration_secs,
                        key_cycle,
                    }),
                    num_keys,
                    total_expected,
                })
            }
            _ => Err(eyre!(
                "--tps and --duration-secs must be provided together for a sustained load test"
            )),
        }
    }

    pub fn is_burst(self) -> bool {
        matches!(self.mode, RunMode::Burst { .. })
    }

    pub fn sustained(self) -> Option<SustainedPlan> {
        match self.mode {
            RunMode::Burst { .. } => None,
            RunMode::Sustained(plan) => Some(plan),
        }
    }

    pub fn transactions_per_key(self) -> u64 {
        match self.mode {
            RunMode::Burst { .. } => 1,
            RunMode::Sustained(SustainedPlan {
                duration_secs,
                key_cycle,
                ..
            }) => duration_secs.div_ceil(key_cycle as u64),
        }
    }
}

fn nonzero(value: u64, flag: &str) -> Result<NonZeroU64> {
    NonZeroU64::new(value).ok_or_else(|| eyre!("{flag} must be greater than zero"))
}

#[cfg(test)]
mod tests {
    use super::{RunMode, RunSizing, SustainedPlan};

    #[test]
    fn sizes_burst_run() {
        let sizing = RunSizing::from_values(4, None, None, 3).unwrap();

        assert_eq!(sizing.mode, RunMode::Burst { num_txs: 4 });
        assert_eq!(sizing.num_keys, 4);
        assert_eq!(sizing.total_expected, 4);
    }

    #[test]
    fn sizes_valid_sustained_run() {
        let sizing = RunSizing::from_values(1, Some(4), Some(10), 3).unwrap();

        assert_eq!(
            sizing.mode,
            RunMode::Sustained(SustainedPlan {
                tps: 4,
                duration_secs: 10,
                key_cycle: 3,
            })
        );
        assert_eq!(sizing.num_keys, 12);
        assert_eq!(sizing.total_expected, 40);
    }

    #[test]
    fn rejects_unpaired_sustained_flags() {
        for result in [
            RunSizing::from_values(1, Some(2), None, 3),
            RunSizing::from_values(1, None, Some(2), 3),
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("must be provided together")
            );
        }
    }

    #[test]
    fn rejects_zero_values() {
        for (result, flag) in [
            (RunSizing::from_values(0, None, None, 3), "--num-txs"),
            (RunSizing::from_values(1, Some(0), Some(2), 3), "--tps"),
            (
                RunSizing::from_values(1, Some(2), Some(0), 3),
                "--duration-secs",
            ),
            (
                RunSizing::from_values(1, Some(2), Some(2), 0),
                "--key-cycle",
            ),
        ] {
            let message = result.unwrap_err().to_string();
            assert!(message.contains(flag), "{message}");
            assert!(message.contains("greater than zero"), "{message}");
        }
    }

    #[test]
    fn rejects_overflow() {
        let error = RunSizing::from_values(1, Some(u64::MAX), Some(2), 1).unwrap_err();

        assert!(error.to_string().contains("supported transaction count"));
    }
}
