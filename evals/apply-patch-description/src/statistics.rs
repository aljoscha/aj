//! Predeclared paired estimators, deterministic bootstrap, and sample planning.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::rng::CounterRng;

/// Invalid statistical input.
#[derive(Debug)]
pub struct StatisticsError(pub String);

impl fmt::Display for StatisticsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for StatisticsError {}

/// Binary outcomes for one paired task instance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BinaryPair {
    pub current: bool,
    pub compact: bool,
}

/// Fixed weighted archetype containing paired binary outcomes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BinaryStratum {
    pub archetype_id: String,
    pub weight: f64,
    pub pairs: Vec<BinaryPair>,
}

/// One-sided confidence bounds for `compact - current`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct RiskDifferenceBounds {
    pub estimate: f64,
    pub lower: f64,
    pub upper: f64,
    pub alpha: f64,
}

/// Computes a conservative fixed-stratum paired risk-difference bound.
///
/// The bound applies Hoeffding's concentration inequality to paired differences in
/// `[-1, 1]`, with each archetype's declared fixed weight. It is
/// distribution-free and intentionally conservative. Unlike a plug-in normal
/// interval, it retains nonzero uncertainty when all paired outcomes agree.
pub fn paired_risk_difference_bounds(
    strata: &[BinaryStratum],
    alpha: f64,
) -> Result<RiskDifferenceBounds, StatisticsError> {
    validate_alpha(alpha)?;
    validate_weights(
        strata
            .iter()
            .map(|stratum| (stratum.weight, stratum.pairs.len())),
    )?;
    let estimate = strata
        .iter()
        .map(|stratum| {
            let difference_sum = stratum
                .pairs
                .iter()
                .map(|pair| f64::from(pair.compact) - f64::from(pair.current))
                .sum::<f64>();
            stratum.weight * difference_sum / usize_as_f64(stratum.pairs.len())
        })
        .sum::<f64>();
    let variance_proxy = strata
        .iter()
        .map(|stratum| stratum.weight.powi(2) / usize_as_f64(stratum.pairs.len()))
        .sum::<f64>();
    let radius = (2.0 * variance_proxy * (1.0 / alpha).ln()).sqrt();
    Ok(RiskDifferenceBounds {
        estimate,
        lower: (estimate - radius).max(-1.0),
        upper: (estimate + radius).min(1.0),
        alpha,
    })
}

/// Paired positive values for one efficiency endpoint.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct EfficiencyPair {
    pub current: f64,
    pub compact: f64,
}

/// Fixed weighted archetype containing paired efficiency values.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EfficiencyStratum {
    pub archetype_id: String,
    pub weight: f64,
    pub pairs: Vec<EfficiencyPair>,
}

/// Deterministic paired bootstrap controls.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BootstrapConfig {
    pub replicates: u32,
    pub seed: String,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            replicates: 100_000,
            seed: "apply-patch-efficiency-bootstrap-v1".into(),
        }
    }
}

/// Relative change and nearest-rank one-sided upper quantiles.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct BootstrapSummary {
    pub relative_change: f64,
    pub upper_95: f64,
    pub upper_97_5: f64,
    pub replicates: u32,
}

/// Resamples whole pairs within every fixed archetype.
pub fn paired_relative_change_bootstrap(
    strata: &[EfficiencyStratum],
    config: &BootstrapConfig,
) -> Result<BootstrapSummary, StatisticsError> {
    if config.replicates == 0 {
        return Err(StatisticsError(
            "bootstrap requires at least one replicate".into(),
        ));
    }
    validate_weights(
        strata
            .iter()
            .map(|stratum| (stratum.weight, stratum.pairs.len())),
    )?;
    validate_efficiency(strata)?;
    let relative_change = relative_change(strata.iter().map(|stratum| {
        let count = usize_as_f64(stratum.pairs.len());
        let current = stratum.pairs.iter().map(|pair| pair.current).sum::<f64>() / count;
        let compact = stratum.pairs.iter().map(|pair| pair.compact).sum::<f64>() / count;
        (stratum.weight, current, compact)
    }))?;

    let mut rng = CounterRng::new(b"paired-efficiency-bootstrap-v1", &[config.seed.as_bytes()]);
    let mut draws = Vec::with_capacity(usize::try_from(config.replicates).unwrap());
    for _ in 0..config.replicates {
        let mut weighted_current = 0.0;
        let mut weighted_compact = 0.0;
        for stratum in strata {
            let mut current = 0.0;
            let mut compact = 0.0;
            for _ in 0..stratum.pairs.len() {
                let selected =
                    usize::try_from(rng.bounded(u64::try_from(stratum.pairs.len()).unwrap()))
                        .unwrap();
                current += stratum.pairs[selected].current;
                compact += stratum.pairs[selected].compact;
            }
            let count = usize_as_f64(stratum.pairs.len());
            weighted_current += stratum.weight * current / count;
            weighted_compact += stratum.weight * compact / count;
        }
        if weighted_current <= 0.0 {
            return Err(StatisticsError(
                "bootstrap sampled a nonpositive current mean".into(),
            ));
        }
        draws.push(weighted_compact / weighted_current - 1.0);
    }
    draws.sort_by(|left, right| left.total_cmp(right));
    Ok(BootstrapSummary {
        relative_change,
        upper_95: nearest_rank(&draws, 0.95),
        upper_97_5: nearest_rank(&draws, 0.975),
        replicates: config.replicates,
    })
}

fn validate_efficiency(strata: &[EfficiencyStratum]) -> Result<(), StatisticsError> {
    if strata
        .iter()
        .flat_map(|stratum| &stratum.pairs)
        .any(|pair| {
            !pair.current.is_finite()
                || !pair.compact.is_finite()
                || pair.current < 0.0
                || pair.compact < 0.0
        })
    {
        return Err(StatisticsError(
            "efficiency values must be finite and nonnegative".into(),
        ));
    }
    Ok(())
}

fn relative_change(values: impl Iterator<Item = (f64, f64, f64)>) -> Result<f64, StatisticsError> {
    let (current, compact) = values.fold((0.0, 0.0), |(current, compact), (weight, a, b)| {
        (current + weight * a, compact + weight * b)
    });
    if current <= 0.0 {
        return Err(StatisticsError(
            "current weighted mean must be positive".into(),
        ));
    }
    Ok(compact / current - 1.0)
}

fn nearest_rank(sorted: &[f64], quantile: f64) -> f64 {
    let rank = (quantile * usize_as_f64(sorted.len())).ceil();
    let index = positive_f64_as_usize(rank)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

/// Wilson score interval for a binomial proportion.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct WilsonBounds {
    pub estimate: f64,
    pub lower: f64,
    pub upper: f64,
}

/// Computes two-sided Wilson bounds using the caller's normal critical value.
pub fn wilson_bounds(successes: u64, trials: u64, z: f64) -> Result<WilsonBounds, StatisticsError> {
    if trials == 0 || successes > trials || !z.is_finite() || z <= 0.0 {
        return Err(StatisticsError("invalid Wilson interval inputs".into()));
    }
    let n = u64_as_f64(trials);
    let estimate = u64_as_f64(successes) / n;
    let z_squared = z * z;
    let denominator = 1.0 + z_squared / n;
    let center = (estimate + z_squared / (2.0 * n)) / denominator;
    let radius =
        z * ((estimate * (1.0 - estimate) / n + z_squared / (4.0 * n * n)).sqrt()) / denominator;
    Ok(WilsonBounds {
        estimate,
        lower: (center - radius).max(0.0),
        upper: (center + radius).min(1.0),
    })
}

/// Blinded paired 2 by 2 event counts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PairedEventCounts {
    pub neither: u64,
    pub first_only: u64,
    pub second_only: u64,
    pub both: u64,
}

impl PairedEventCounts {
    /// Returns the number of pairs.
    pub fn pairs(self) -> u64 {
        self.neither + self.first_only + self.second_only + self.both
    }
}

/// Deterministic planner controls.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlannerConfig {
    pub alpha: f64,
    pub target_power: f64,
    pub task_success_margin: f64,
    pub patch_failure_margin: f64,
    pub edit_bypass_margin: f64,
    pub simulation_replicates: u32,
    pub maximum_pairs_per_archetype: u32,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            target_power: 0.8,
            task_success_margin: 0.05,
            patch_failure_margin: 0.03,
            edit_bypass_margin: 0.02,
            simulation_replicates: 10_000,
            maximum_pairs_per_archetype: 20_000,
        }
    }
}

/// Fixed sample recommendation from blinded pilot counts.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SamplePlan {
    pub pairs_per_archetype: u32,
    pub all_zero_minimum: u32,
    pub all_pass_minimum: u32,
    pub simulated_power: f64,
    pub zero_observed_event_probability: f64,
}

/// Plans one common repetition count for all 16 equally weighted archetypes.
pub fn plan_sample(
    config: &PlannerConfig,
    blinded_counts: PairedEventCounts,
    seed: &str,
) -> Result<SamplePlan, StatisticsError> {
    validate_alpha(config.alpha)?;
    if !(0.0..=1.0).contains(&config.target_power)
        || config.simulation_replicates == 0
        || blinded_counts.pairs() == 0
    {
        return Err(StatisticsError(
            "invalid planner controls or empty pilot".into(),
        ));
    }
    let all_pass_minimum = feasibility_repetitions(config.task_success_margin, config.alpha)?;
    let all_zero_minimum = feasibility_repetitions(
        config.patch_failure_margin.min(config.edit_bypass_margin),
        config.alpha,
    )?;
    let mut repetitions = all_pass_minimum.max(all_zero_minimum);
    let zero_observed_event_probability =
        (0.5 / (u64_as_f64(blinded_counts.pairs()) + 1.0)).min(0.5);
    let mut power;
    loop {
        power = simulated_guardrail_power(config, blinded_counts, repetitions, seed)?;
        if power >= config.target_power || repetitions >= config.maximum_pairs_per_archetype {
            break;
        }
        repetitions = repetitions.saturating_add((repetitions / 10).max(1));
    }
    if power < config.target_power {
        return Err(StatisticsError(format!(
            "required sample exceeds maximum {} pairs per archetype",
            config.maximum_pairs_per_archetype
        )));
    }
    Ok(SamplePlan {
        pairs_per_archetype: repetitions,
        all_zero_minimum,
        all_pass_minimum,
        simulated_power: power,
        zero_observed_event_probability,
    })
}

fn feasibility_repetitions(margin: f64, alpha: f64) -> Result<u32, StatisticsError> {
    if !margin.is_finite() || margin <= 0.0 || margin >= 1.0 {
        return Err(StatisticsError(
            "guardrail margins must be between zero and one".into(),
        ));
    }
    // For 16 weights of 1/16, the paired Hoeffding radius is
    // sqrt(2 * ln(1/alpha) / (16 * repetitions)).
    let required = (2.0 * (1.0 / alpha).ln() / (16.0 * margin * margin)).ceil();
    positive_f64_as_u32(required)
}

fn simulated_guardrail_power(
    config: &PlannerConfig,
    counts: PairedEventCounts,
    repetitions: u32,
    seed: &str,
) -> Result<f64, StatisticsError> {
    let total = u64_as_f64(counts.pairs()) + 2.0;
    let discordant = u64_as_f64(counts.first_only + counts.second_only) / 2.0;
    let probabilities = [
        (u64_as_f64(counts.neither) + 0.5) / total,
        (discordant + 0.5) / total,
        (discordant + 0.5) / total,
        (u64_as_f64(counts.both) + 0.5) / total,
    ];
    let mut cumulative = [0.0; 4];
    cumulative[0] = probabilities[0];
    cumulative[1] = cumulative[0] + probabilities[1];
    cumulative[2] = cumulative[1] + probabilities[2];
    cumulative[3] = 1.0;
    let mut rng = CounterRng::new(b"blinded-paired-event-planner-v1", &[seed.as_bytes()]);
    let radius = (2.0 * (1.0 / config.alpha).ln() / (16.0 * f64::from(repetitions))).sqrt();
    let margin = config.patch_failure_margin.min(config.edit_bypass_margin);
    let mut passed = 0_u32;
    for _ in 0..config.simulation_replicates {
        let mut effect = 0.0;
        for _ in 0..16_u8 {
            let mut difference = 0_i64;
            for _ in 0..repetitions {
                let draw = u64_as_f64(rng.next_u64()) / u64_as_f64(u64::MAX);
                if draw < cumulative[0] || draw >= cumulative[2] {
                    continue;
                }
                difference += if draw < cumulative[1] { 1 } else { -1 };
            }
            effect += i64_as_f64(difference) / f64::from(repetitions) / 16.0;
        }
        if effect + radius < margin {
            passed += 1;
        }
    }
    Ok(f64::from(passed) / f64::from(config.simulation_replicates))
}

fn validate_alpha(alpha: f64) -> Result<(), StatisticsError> {
    if !alpha.is_finite() || alpha <= 0.0 || alpha >= 1.0 {
        return Err(StatisticsError("alpha must be between zero and one".into()));
    }
    Ok(())
}

fn validate_weights(values: impl Iterator<Item = (f64, usize)>) -> Result<(), StatisticsError> {
    let values = values.collect::<Vec<_>>();
    if values.is_empty()
        || values
            .iter()
            .any(|(weight, count)| !weight.is_finite() || *weight <= 0.0 || *count == 0)
        || (values.iter().map(|(weight, _)| weight).sum::<f64>() - 1.0).abs() > 1e-12
    {
        return Err(StatisticsError(
            "strata must be nonempty with positive weights summing to one".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::as_conversions)]
fn usize_as_f64(value: usize) -> f64 {
    value as f64
}

#[allow(clippy::as_conversions)]
fn u64_as_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::as_conversions)]
fn i64_as_f64(value: i64) -> f64 {
    value as f64
}

#[allow(clippy::as_conversions)]
fn positive_f64_as_usize(value: f64) -> usize {
    debug_assert!(value.is_finite() && value >= 0.0 && value <= usize::MAX as f64);
    value as usize
}

#[allow(clippy::as_conversions)]
fn positive_f64_as_u32(value: f64) -> Result<u32, StatisticsError> {
    if !value.is_finite() || value < 0.0 || value > f64::from(u32::MAX) {
        return Err(StatisticsError("sample requirement exceeds u32".into()));
    }
    Ok(value as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_strata(pair: BinaryPair) -> Vec<BinaryStratum> {
        (0..16)
            .map(|index| BinaryStratum {
                archetype_id: format!("a{index}"),
                weight: 1.0 / 16.0,
                pairs: vec![pair; 100],
            })
            .collect()
    }

    #[test]
    fn all_zero_and_all_pass_retain_uncertainty() {
        for pair in [
            BinaryPair {
                current: false,
                compact: false,
            },
            BinaryPair {
                current: true,
                compact: true,
            },
        ] {
            let bounds = paired_risk_difference_bounds(&binary_strata(pair), 0.05).unwrap();
            assert_eq!(bounds.estimate, 0.0);
            assert!(bounds.lower < 0.0);
            assert!(bounds.upper > 0.0);
        }
    }

    #[test]
    fn variant_swap_and_complement_are_symmetric() {
        let pairs = [
            BinaryPair {
                current: false,
                compact: true,
            },
            BinaryPair {
                current: true,
                compact: true,
            },
            BinaryPair {
                current: true,
                compact: false,
            },
            BinaryPair {
                current: false,
                compact: true,
            },
        ];
        let make = |pairs: Vec<BinaryPair>| {
            vec![BinaryStratum {
                archetype_id: "x".into(),
                weight: 1.0,
                pairs,
            }]
        };
        let original = paired_risk_difference_bounds(&make(pairs.to_vec()), 0.05).unwrap();
        let swapped = paired_risk_difference_bounds(
            &make(
                pairs
                    .iter()
                    .map(|pair| BinaryPair {
                        current: pair.compact,
                        compact: pair.current,
                    })
                    .collect(),
            ),
            0.05,
        )
        .unwrap();
        let complemented = paired_risk_difference_bounds(
            &make(
                pairs
                    .iter()
                    .map(|pair| BinaryPair {
                        current: !pair.current,
                        compact: !pair.compact,
                    })
                    .collect(),
            ),
            0.05,
        )
        .unwrap();
        assert!((original.estimate + swapped.estimate).abs() < 1e-12);
        assert!((original.lower + swapped.upper).abs() < 1e-12);
        assert!((original.estimate + complemented.estimate).abs() < 1e-12);
        assert!((original.upper + complemented.lower).abs() < 1e-12);
    }

    #[test]
    fn honors_fixed_stratum_weights() {
        let strata = vec![
            BinaryStratum {
                archetype_id: "heavy".into(),
                weight: 0.75,
                pairs: vec![
                    BinaryPair {
                        current: false,
                        compact: true
                    };
                    20
                ],
            },
            BinaryStratum {
                archetype_id: "light".into(),
                weight: 0.25,
                pairs: vec![
                    BinaryPair {
                        current: true,
                        compact: false
                    };
                    20
                ],
            },
        ];
        assert_eq!(
            paired_risk_difference_bounds(&strata, 0.05)
                .unwrap()
                .estimate,
            0.5
        );
    }

    #[test]
    fn deterministic_bootstrap_has_golden_quantiles() {
        let strata = vec![EfficiencyStratum {
            archetype_id: "one".into(),
            weight: 1.0,
            pairs: vec![
                EfficiencyPair {
                    current: 10.0,
                    compact: 8.0,
                },
                EfficiencyPair {
                    current: 20.0,
                    compact: 21.0,
                },
                EfficiencyPair {
                    current: 30.0,
                    compact: 24.0,
                },
            ],
        }];
        let summary = paired_relative_change_bootstrap(
            &strata,
            &BootstrapConfig {
                replicates: 10_000,
                seed: "golden".into(),
            },
        )
        .unwrap();
        assert!((summary.relative_change + 0.116_666_666_666_666_7).abs() < 1e-12);
        assert!(summary.upper_95.abs() < 1e-12);
        assert!((summary.upper_97_5 - 0.05).abs() < 1e-12);
    }

    #[test]
    fn wilson_never_collapses_at_extremes() {
        let zero = wilson_bounds(0, 20, 1.96).unwrap();
        let all = wilson_bounds(20, 20, 1.96).unwrap();
        assert!(zero.upper > 0.0);
        assert!(all.lower < 1.0);
        assert!((zero.upper + all.lower - 1.0).abs() < 1e-12);
    }

    #[test]
    fn planner_smooths_zero_events_and_enforces_feasibility() {
        let config = PlannerConfig {
            simulation_replicates: 100,
            ..PlannerConfig::default()
        };
        let plan = plan_sample(
            &config,
            PairedEventCounts {
                neither: 48,
                ..PairedEventCounts::default()
            },
            "planner",
        )
        .unwrap();
        assert!(plan.zero_observed_event_probability > 0.0);
        assert!(plan.pairs_per_archetype >= plan.all_zero_minimum);
        assert!(plan.pairs_per_archetype >= plan.all_pass_minimum);

        let rare = (0..16)
            .map(|index| BinaryStratum {
                archetype_id: format!("a{index}"),
                weight: 1.0 / 16.0,
                pairs: vec![
                    BinaryPair {
                        current: false,
                        compact: false,
                    };
                    usize::try_from(plan.pairs_per_archetype).unwrap()
                ],
            })
            .collect::<Vec<_>>();
        let all_pass = (0..16)
            .map(|index| BinaryStratum {
                archetype_id: format!("a{index}"),
                weight: 1.0 / 16.0,
                pairs: vec![
                    BinaryPair {
                        current: true,
                        compact: true,
                    };
                    usize::try_from(plan.pairs_per_archetype).unwrap()
                ],
            })
            .collect::<Vec<_>>();
        assert!(
            paired_risk_difference_bounds(&rare, config.alpha)
                .unwrap()
                .upper
                < 0.02
        );
        assert!(
            paired_risk_difference_bounds(&all_pass, config.alpha)
                .unwrap()
                .lower
                > -0.05
        );
    }
}
