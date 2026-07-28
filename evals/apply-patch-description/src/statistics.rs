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

const ONE_SIDED_95_Z: f64 = 1.644_853_626_951_472_2;
const OPTIMIZER_TOLERANCE: f64 = 1e-12;
const MAX_OPTIMIZER_ITERATIONS: usize = 128;
const COMPONENT_SEARCH_STEPS: usize = 128;

#[derive(Clone, Copy, Debug)]
struct BinaryCounts {
    n00: f64,
    n01: f64,
    n10: f64,
    n11: f64,
    weight: f64,
}

impl BinaryCounts {
    fn pairs(self) -> f64 {
        self.n00 + self.n01 + self.n10 + self.n11
    }

    fn estimate(self) -> f64 {
        (self.n01 - self.n10) / self.pairs()
    }
}

#[derive(Clone, Copy, Debug)]
struct NuisanceEstimate {
    difference: f64,
    discordance: f64,
}

/// Computes the fixed-archetype paired profile-score bounds.
///
/// The multinomial nuisance parameters are fitted under each candidate weighted
/// risk difference. The returned lower and upper limits are both one-sided 95%
/// bounds from the acceptance component containing the point estimate.
pub fn paired_risk_difference_bounds(
    strata: &[BinaryStratum],
    alpha: f64,
) -> Result<RiskDifferenceBounds, StatisticsError> {
    validate_alpha(alpha)?;
    if (alpha - 0.05).abs() > f64::EPSILON {
        return Err(StatisticsError(
            "paired profile-score bounds are predeclared only for alpha = 0.05".into(),
        ));
    }
    validate_weights(
        strata
            .iter()
            .map(|stratum| (stratum.weight, stratum.pairs.len())),
    )?;
    let counts = strata.iter().map(binary_counts).collect::<Vec<_>>();
    let estimate = counts
        .iter()
        .map(|stratum| stratum.weight * stratum.estimate())
        .sum::<f64>();
    if !estimate.is_finite() || !(-1.0..=1.0).contains(&estimate) {
        return Err(StatisticsError(
            "paired risk-difference estimate is not finite or feasible".into(),
        ));
    }
    let lower = invert_profile_score(&counts, estimate, -1.0)?;
    let upper = invert_profile_score(&counts, estimate, 1.0)?;
    Ok(RiskDifferenceBounds {
        estimate,
        lower,
        upper,
        alpha,
    })
}

/// Tests whether the one-sided profile-score upper bound excludes `margin`.
///
/// This is the decision-equivalent form of inverting the production score test.
#[cfg(test)]
fn paired_upper_bound_below(
    strata: &[BinaryStratum],
    margin: f64,
    alpha: f64,
) -> Result<bool, StatisticsError> {
    validate_alpha(alpha)?;
    if (alpha - 0.05).abs() > f64::EPSILON || !(-1.0..=1.0).contains(&margin) {
        return Err(StatisticsError(
            "paired profile-score decisions require alpha = 0.05 and a feasible margin".into(),
        ));
    }
    validate_weights(
        strata
            .iter()
            .map(|stratum| (stratum.weight, stratum.pairs.len())),
    )?;
    let counts = strata.iter().map(binary_counts).collect::<Vec<_>>();
    let estimate = counts
        .iter()
        .map(|stratum| stratum.weight * stratum.estimate())
        .sum::<f64>();
    if estimate >= margin {
        return Ok(false);
    }
    Ok(profile_score_magnitude(&counts, estimate, margin)? > ONE_SIDED_95_Z)
}

fn binary_counts(stratum: &BinaryStratum) -> BinaryCounts {
    let mut counts = BinaryCounts {
        n00: 0.0,
        n01: 0.0,
        n10: 0.0,
        n11: 0.0,
        weight: stratum.weight,
    };
    for pair in &stratum.pairs {
        match (pair.current, pair.compact) {
            (false, false) => counts.n00 += 1.0,
            (false, true) => counts.n01 += 1.0,
            (true, false) => counts.n10 += 1.0,
            (true, true) => counts.n11 += 1.0,
        }
    }
    counts
}

fn invert_profile_score(
    counts: &[BinaryCounts],
    estimate: f64,
    endpoint: f64,
) -> Result<f64, StatisticsError> {
    if estimate == endpoint {
        return Ok(endpoint);
    }

    let mut accepted = estimate;
    let mut rejected = endpoint;
    let distance = endpoint - estimate;
    for step in 1..=COMPONENT_SEARCH_STEPS {
        let fraction = usize_as_f64(step) / usize_as_f64(COMPONENT_SEARCH_STEPS);
        let candidate = estimate + distance * fraction;
        if profile_score_magnitude(counts, estimate, candidate)? > ONE_SIDED_95_Z {
            rejected = candidate;
            break;
        }
        accepted = candidate;
    }

    if accepted == endpoint {
        return Ok(endpoint);
    }
    for _ in 0..MAX_OPTIMIZER_ITERATIONS {
        let candidate = accepted.midpoint(rejected);
        if profile_score_magnitude(counts, estimate, candidate)? <= ONE_SIDED_95_Z {
            accepted = candidate;
        } else {
            rejected = candidate;
        }
        if (accepted - rejected).abs() <= OPTIMIZER_TOLERANCE {
            break;
        }
    }
    Ok(accepted.midpoint(rejected))
}

fn profile_score_magnitude(
    counts: &[BinaryCounts],
    estimate: f64,
    candidate: f64,
) -> Result<f64, StatisticsError> {
    if candidate == estimate {
        return Ok(0.0);
    }
    if candidate == -1.0 || candidate == 1.0 {
        return Ok(f64::INFINITY);
    }
    let nuisance = constrained_nuisance_mle(counts, candidate)?;
    let mut variance = 0.0;
    for (stratum, nuisance) in counts.iter().zip(nuisance) {
        let contribution = nuisance.discordance - nuisance.difference.powi(2);
        if !contribution.is_finite() || contribution < 0.0 {
            return Err(StatisticsError(format!(
                "profile-score optimizer produced invalid null variance contribution {contribution}"
            )));
        }
        variance += stratum.weight.powi(2) / stratum.pairs() * contribution;
    }
    if !variance.is_finite() || variance < 0.0 {
        return Err(StatisticsError(
            "profile-score optimizer produced an invalid null variance".into(),
        ));
    }
    if variance == 0.0 {
        return Ok(f64::INFINITY);
    }
    Ok((estimate - candidate).abs() / variance.sqrt())
}

fn constrained_nuisance_mle(
    counts: &[BinaryCounts],
    candidate: f64,
) -> Result<Vec<NuisanceEstimate>, StatisticsError> {
    if !candidate.is_finite() || !(-1.0..=1.0).contains(&candidate) {
        return Err(StatisticsError(format!(
            "candidate risk difference {candidate} is not feasible"
        )));
    }
    let estimate = counts
        .iter()
        .map(|stratum| stratum.weight * stratum.estimate())
        .sum::<f64>();
    if candidate == estimate {
        return Ok(counts
            .iter()
            .map(|stratum| NuisanceEstimate {
                difference: stratum.estimate(),
                discordance: (stratum.n01 + stratum.n10) / stratum.pairs(),
            })
            .collect());
    }

    let direction = if candidate < estimate { -1.0 } else { 1.0 };
    let mut inner_multiplier = 0.0;
    let mut outer_multiplier = counts
        .iter()
        .map(|stratum| stratum.pairs() / stratum.weight)
        .fold(1.0, f64::max)
        * direction;
    let mut outer = maximize_strata(counts, outer_multiplier)?;
    for _ in 0..MAX_OPTIMIZER_ITERATIONS {
        let outer_difference = weighted_difference(counts, &outer);
        if (direction < 0.0 && outer_difference <= candidate)
            || (direction > 0.0 && outer_difference >= candidate)
        {
            break;
        }
        inner_multiplier = outer_multiplier;
        outer_multiplier *= 2.0;
        if !outer_multiplier.is_finite() {
            return Err(StatisticsError(
                "profile-score Lagrange multiplier failed to bracket the constraint".into(),
            ));
        }
        outer = maximize_strata(counts, outer_multiplier)?;
    }
    let outer_difference = weighted_difference(counts, &outer);
    if (direction < 0.0 && outer_difference > candidate)
        || (direction > 0.0 && outer_difference < candidate)
    {
        return Err(StatisticsError(format!(
            "profile-score Lagrange multiplier did not bracket candidate {candidate}"
        )));
    }

    let (mut lower_multiplier, mut upper_multiplier) = if direction < 0.0 {
        (outer_multiplier, inner_multiplier)
    } else {
        (inner_multiplier, outer_multiplier)
    };
    let mut solution = outer;
    for _ in 0..MAX_OPTIMIZER_ITERATIONS {
        let multiplier = lower_multiplier.midpoint(upper_multiplier);
        let nuisance = maximize_strata(counts, multiplier)?;
        let difference = weighted_difference(counts, &nuisance);
        solution = nuisance;
        if (difference - candidate).abs() <= OPTIMIZER_TOLERANCE {
            break;
        }
        if difference < candidate {
            lower_multiplier = multiplier;
        } else {
            upper_multiplier = multiplier;
        }
    }
    let residual = (weighted_difference(counts, &solution) - candidate).abs();
    if !residual.is_finite() || residual > 5.0 * OPTIMIZER_TOLERANCE {
        return Err(StatisticsError(format!(
            "profile-score nuisance constraint did not converge for candidate {candidate}: residual {residual}"
        )));
    }
    Ok(solution)
}

fn maximize_strata(
    counts: &[BinaryCounts],
    multiplier: f64,
) -> Result<Vec<NuisanceEstimate>, StatisticsError> {
    counts
        .iter()
        .map(|stratum| maximize_stratum(*stratum, multiplier * stratum.weight))
        .collect()
}

fn maximize_stratum(counts: BinaryCounts, tilt: f64) -> Result<NuisanceEstimate, StatisticsError> {
    if !tilt.is_finite() {
        return Err(StatisticsError(
            "profile-score stratum tilt is not finite".into(),
        ));
    }

    // Profiling `p00` and `p11` leaves three simplex cells with counts
    // `n01`, `n10`, and `n00 + n11`. Their linear tilts are λw, -λw, and 0.
    let cell_counts = [counts.n01, counts.n10, counts.n00 + counts.n11];
    let tilts = [tilt, -tilt, 0.0];
    let active_max = cell_counts
        .iter()
        .zip(tilts)
        .filter(|(count, _)| **count > 0.0)
        .map(|(_, tilt)| tilt)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut lower = active_max;
    let mut upper = active_max + counts.pairs();
    if !lower.is_finite() || !upper.is_finite() || upper <= lower {
        return Err(StatisticsError(
            "profile-score stratum optimizer could not bracket its concave maximum".into(),
        ));
    }
    for _ in 0..MAX_OPTIMIZER_ITERATIONS {
        let candidate = lower.midpoint(upper);
        let mass = cell_counts
            .iter()
            .zip(tilts)
            .filter(|(count, _)| **count > 0.0)
            .map(|(count, tilt)| count / (candidate - tilt))
            .sum::<f64>();
        if !mass.is_finite() || mass <= 0.0 {
            return Err(StatisticsError(
                "profile-score stratum optimizer encountered invalid probability mass".into(),
            ));
        }
        if mass > 1.0 {
            lower = candidate;
        } else {
            upper = candidate;
        }
    }

    let root = upper;
    let maximum_tilt = tilt.abs();
    let level = root.max(maximum_tilt);
    let mut probabilities = [0.0; 3];
    for index in 0..3 {
        if cell_counts[index] > 0.0 {
            let denominator = level - tilts[index];
            if !denominator.is_finite() || denominator <= 0.0 {
                return Err(StatisticsError(
                    "profile-score stratum optimizer reached an invalid boundary".into(),
                ));
            }
            probabilities[index] = cell_counts[index] / denominator;
        }
    }
    let assigned = probabilities.iter().sum::<f64>();
    if !assigned.is_finite() || assigned > 1.0 + OPTIMIZER_TOLERANCE {
        return Err(StatisticsError(
            "profile-score stratum optimizer produced infeasible probabilities".into(),
        ));
    }
    if level > root {
        let boundary = tilts
            .iter()
            .enumerate()
            .find(|(index, cell_tilt)| cell_counts[*index] == 0.0 && **cell_tilt == maximum_tilt)
            .map(|(index, _)| index)
            .ok_or_else(|| {
                StatisticsError("profile-score stratum optimizer lost its active boundary".into())
            })?;
        probabilities[boundary] += 1.0 - assigned;
    } else {
        for probability in &mut probabilities {
            *probability /= assigned;
        }
    }

    let difference = probabilities[0] - probabilities[1];
    let discordance = probabilities[0] + probabilities[1];
    if !difference.is_finite()
        || !discordance.is_finite()
        || difference.abs() > discordance + OPTIMIZER_TOLERANCE
        || discordance > 1.0 + OPTIMIZER_TOLERANCE
    {
        return Err(StatisticsError(
            "profile-score stratum optimizer produced an infeasible nuisance estimate".into(),
        ));
    }
    Ok(NuisanceEstimate {
        difference,
        discordance,
    })
}

fn weighted_difference(counts: &[BinaryCounts], nuisance: &[NuisanceEstimate]) -> f64 {
    counts
        .iter()
        .zip(nuisance)
        .map(|(stratum, nuisance)| stratum.weight * nuisance.difference)
        .sum()
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

/// Relative change and nearest-rank one-sided quantiles.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct BootstrapSummary {
    pub defined: bool,
    pub bounds_defined: bool,
    pub relative_change: f64,
    pub lower_95: f64,
    pub lower_97_5: f64,
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
    let (weighted_current, weighted_compact) = strata.iter().fold((0.0, 0.0), |total, stratum| {
        let count = usize_as_f64(stratum.pairs.len());
        let current = stratum.pairs.iter().map(|pair| pair.current).sum::<f64>() / count;
        let compact = stratum.pairs.iter().map(|pair| pair.compact).sum::<f64>() / count;
        (
            total.0 + stratum.weight * current,
            total.1 + stratum.weight * compact,
        )
    });
    if weighted_current <= 0.0 {
        return Ok(undefined_bootstrap(config.replicates));
    }
    let relative_change = weighted_compact / weighted_current - 1.0;

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
            return Ok(BootstrapSummary {
                defined: true,
                bounds_defined: false,
                relative_change,
                lower_95: 0.0,
                lower_97_5: 0.0,
                upper_95: 0.0,
                upper_97_5: 0.0,
                replicates: config.replicates,
            });
        }
        draws.push(weighted_compact / weighted_current - 1.0);
    }
    draws.sort_by(|left, right| left.total_cmp(right));
    Ok(BootstrapSummary {
        defined: true,
        bounds_defined: true,
        relative_change,
        lower_95: nearest_rank(&draws, 0.05),
        lower_97_5: nearest_rank(&draws, 0.025),
        upper_95: nearest_rank(&draws, 0.95),
        upper_97_5: nearest_rank(&draws, 0.975),
        replicates: config.replicates,
    })
}

fn undefined_bootstrap(replicates: u32) -> BootstrapSummary {
    BootstrapSummary {
        defined: false,
        bounds_defined: false,
        relative_change: 0.0,
        lower_95: 0.0,
        lower_97_5: 0.0,
        upper_95: 0.0,
        upper_97_5: 0.0,
        replicates,
    }
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

/// Blinded paired values for the two efficiency endpoints in one pilot pair.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlindedEfficiencyPair {
    pub first_cost: f64,
    pub second_cost: f64,
    pub first_responses: u64,
    pub second_responses: u64,
}

/// Label-free pilot inputs retained for one archetype.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlindedPlannerStratum {
    pub archetype_id: String,
    pub task_failure: PairedEventCounts,
    pub patch_failure: PairedEventCounts,
    pub edit_bypass: PairedEventCounts,
    pub efficiency: Vec<BlindedEfficiencyPair>,
}

/// Complete label-free pilot input accepted by the sample planner.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlindedPlannerInput {
    pub strata: Vec<BlindedPlannerStratum>,
}

/// Deterministic planner controls frozen into the planning report.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlannerConfig {
    pub alpha: f64,
    pub target_power: f64,
    pub task_success_margin: f64,
    pub patch_failure_margin: f64,
    pub edit_bypass_margin: f64,
    pub worthwhile_efficiency_improvement: f64,
    pub efficiency_non_degradation_margin: f64,
    pub simulation_replicates: u32,
    pub maximum_pairs_per_archetype: u32,
    pub power_confidence_z: f64,
    pub variance_upper_confidence: f64,
    pub planner_version: String,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            target_power: 0.8,
            task_success_margin: 0.05,
            patch_failure_margin: 0.03,
            edit_bypass_margin: 0.02,
            worthwhile_efficiency_improvement: 0.05,
            efficiency_non_degradation_margin: 0.02,
            simulation_replicates: 512,
            maximum_pairs_per_archetype: 508,
            power_confidence_z: ONE_SIDED_95_Z,
            variance_upper_confidence: 0.95,
            planner_version: "paired-joint-wilson-planner-v4".into(),
        }
    }
}

/// Planner result for one guardrail or efficiency alternative.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EndpointSampleRequirement {
    pub endpoint: String,
    pub required_pairs_per_archetype: Option<u32>,
    pub achieved_power: f64,
    pub achieved_power_lower_bound: f64,
    pub all_extreme_minimum: Option<u32>,
    pub nuisance_points_evaluated: u32,
}

/// Whether every predeclared endpoint fits the practical frozen universe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanningConclusion {
    Recommended,
    InconclusiveInsufficientUniverse,
}

/// Fixed common sample recommendation from all blinded pilot endpoints.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SamplePlan {
    pub conclusion: PlanningConclusion,
    pub pairs_per_archetype: Option<u32>,
    pub limiting_endpoint: Option<String>,
    pub endpoint_requirements: Vec<EndpointSampleRequirement>,
    pub target_power: f64,
    pub practical_cap: u32,
}

/// Plans one common repetition count across all binary and efficiency endpoints.
pub fn plan_sample(
    config: &PlannerConfig,
    pilot: &BlindedPlannerInput,
    seed: &str,
) -> Result<SamplePlan, StatisticsError> {
    validate_planner(config, pilot)?;
    let binary = [
        (
            "task_success",
            config.task_success_margin,
            event_counts(pilot, |stratum| stratum.task_failure),
        ),
        (
            "sessions_with_patch_failure",
            config.patch_failure_margin,
            event_counts(pilot, |stratum| stratum.patch_failure),
        ),
        (
            "edit_bypass",
            config.edit_bypass_margin,
            event_counts(pilot, |stratum| stratum.edit_bypass),
        ),
    ];
    let mut endpoint_requirements = Vec::new();
    for (endpoint, margin, counts) in binary {
        endpoint_requirements.push(plan_binary_endpoint(
            config, endpoint, margin, counts, seed,
        )?);
    }
    endpoint_requirements.extend(plan_efficiency_alternatives(config, pilot, seed)?);

    let limiting = endpoint_requirements
        .iter()
        .filter_map(|requirement| {
            requirement
                .required_pairs_per_archetype
                .map(|pairs| (pairs, requirement.endpoint.clone()))
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)));
    let insufficient = endpoint_requirements
        .iter()
        .any(|requirement| requirement.required_pairs_per_archetype.is_none());
    let (pairs_per_archetype, limiting_endpoint) = if insufficient {
        (None, None)
    } else {
        let (pairs, endpoint) = limiting
            .ok_or_else(|| StatisticsError("planner produced no endpoint requirements".into()))?;
        (Some(pairs), Some(endpoint))
    };
    Ok(SamplePlan {
        conclusion: if insufficient {
            PlanningConclusion::InconclusiveInsufficientUniverse
        } else {
            PlanningConclusion::Recommended
        },
        pairs_per_archetype,
        limiting_endpoint,
        endpoint_requirements,
        target_power: config.target_power,
        practical_cap: config.maximum_pairs_per_archetype,
    })
}

pub(crate) fn validate_planner(
    config: &PlannerConfig,
    pilot: &BlindedPlannerInput,
) -> Result<(), StatisticsError> {
    validate_alpha(config.alpha)?;
    if (config.alpha - 0.05).abs() > f64::EPSILON
        || !(0.0..1.0).contains(&config.target_power)
        || config.target_power < 0.8
        || (config.task_success_margin - 0.05).abs() > f64::EPSILON
        || (config.patch_failure_margin - 0.03).abs() > f64::EPSILON
        || (config.edit_bypass_margin - 0.02).abs() > f64::EPSILON
        || (config.worthwhile_efficiency_improvement - 0.05).abs() > f64::EPSILON
        || (config.efficiency_non_degradation_margin - 0.02).abs() > f64::EPSILON
        || config.simulation_replicates != 512
        || config.maximum_pairs_per_archetype == 0
        || (config.power_confidence_z - ONE_SIDED_95_Z).abs() > f64::EPSILON
        || (config.variance_upper_confidence - 0.95).abs() > f64::EPSILON
        || config.planner_version != "paired-joint-wilson-planner-v4"
        || pilot.strata.len() != 16
    {
        return Err(StatisticsError(
            "invalid planner controls or empty pilot".into(),
        ));
    }
    for margin in [
        config.task_success_margin,
        config.patch_failure_margin,
        config.edit_bypass_margin,
        config.worthwhile_efficiency_improvement,
        config.efficiency_non_degradation_margin,
    ] {
        if !margin.is_finite() || !(0.0..1.0).contains(&margin) {
            return Err(StatisticsError(
                "planner margins must be finite and between zero and one".into(),
            ));
        }
    }
    let pilot_pairs = pilot.strata[0].efficiency.len();
    if pilot_pairs != 3
        || pilot.strata.iter().any(|stratum| {
            stratum.efficiency.len() != pilot_pairs
                || stratum.task_failure.pairs() != u64::try_from(pilot_pairs).unwrap()
                || stratum.patch_failure.pairs() != u64::try_from(pilot_pairs).unwrap()
                || stratum.edit_bypass.pairs() != u64::try_from(pilot_pairs).unwrap()
        })
    {
        return Err(StatisticsError(
            "every planner stratum must contain the frozen three pilot pairs".into(),
        ));
    }
    if pilot
        .strata
        .iter()
        .flat_map(|stratum| &stratum.efficiency)
        .any(|pair| {
            [pair.first_cost, pair.second_cost]
                .into_iter()
                .any(|value| !value.is_finite() || value < 0.0)
        })
    {
        return Err(StatisticsError(
            "planner cost values must be finite and nonnegative".into(),
        ));
    }
    Ok(())
}

fn event_counts(
    pilot: &BlindedPlannerInput,
    field: impl Fn(&BlindedPlannerStratum) -> PairedEventCounts,
) -> PairedEventCounts {
    pilot
        .strata
        .iter()
        .map(field)
        .fold(PairedEventCounts::default(), |mut total, counts| {
            total.neither += counts.neither;
            total.first_only += counts.first_only;
            total.second_only += counts.second_only;
            total.both += counts.both;
            total
        })
}

fn plan_binary_endpoint(
    config: &PlannerConfig,
    endpoint: &str,
    margin: f64,
    counts: PairedEventCounts,
    seed: &str,
) -> Result<EndpointSampleRequirement, StatisticsError> {
    let nuisance = conservative_nuisance_points(counts)?;
    let all_extreme_minimum = all_zero_minimum(
        u32::try_from(16).unwrap_or(16),
        margin,
        config.maximum_pairs_per_archetype,
    )?;
    let Some(minimum) = all_extreme_minimum else {
        return Ok(EndpointSampleRequirement {
            endpoint: endpoint.into(),
            required_pairs_per_archetype: None,
            achieved_power: 0.0,
            achieved_power_lower_bound: 0.0,
            all_extreme_minimum: None,
            nuisance_points_evaluated: u32::try_from(nuisance.len()).unwrap(),
        });
    };
    let power = |pairs| binary_power(config, endpoint, margin, pairs, &nuisance, seed);
    let (required, achieved, achieved_lower) = find_required_sample(config, minimum, power)?;
    Ok(EndpointSampleRequirement {
        endpoint: endpoint.into(),
        required_pairs_per_archetype: required,
        achieved_power: achieved,
        achieved_power_lower_bound: achieved_lower,
        all_extreme_minimum: Some(minimum),
        nuisance_points_evaluated: u32::try_from(nuisance.len()).unwrap(),
    })
}

fn conservative_nuisance_points(
    counts: PairedEventCounts,
) -> Result<Vec<[f64; 4]>, StatisticsError> {
    let pairs = counts.pairs();
    if pairs == 0 {
        return Err(StatisticsError("empty paired event table".into()));
    }
    let events = counts.first_only + counts.second_only + 2 * counts.both;
    let any_event = counts.first_only + counts.second_only + counts.both;
    let both_event = counts.both;
    // We bound the marginal event rate by the paired any-event and both-event
    // rates. Treating the two correlated sides as 2n binomial trials is narrower.
    let event_bounds = WilsonBounds {
        estimate: u64_as_f64(events) / (2.0 * u64_as_f64(pairs)),
        lower: wilson_bounds(both_event, pairs, ONE_SIDED_95_Z)?.lower,
        upper: wilson_bounds(any_event, pairs, ONE_SIDED_95_Z)?.upper,
    };
    let discordant = counts.first_only + counts.second_only;
    let discordance_bounds = wilson_bounds(discordant, pairs, ONE_SIDED_95_Z)?;
    let mut points = Vec::new();
    for event_rate in [
        event_bounds.lower,
        event_bounds.estimate,
        event_bounds.upper,
    ] {
        for discordance in [
            discordance_bounds.lower,
            discordance_bounds.estimate,
            discordance_bounds.upper,
        ] {
            let discordance = discordance.min(2.0 * event_rate.min(1.0 - event_rate));
            let probabilities = [
                1.0 - event_rate - discordance / 2.0,
                discordance / 2.0,
                discordance / 2.0,
                event_rate - discordance / 2.0,
            ];
            if probabilities.iter().all(|probability| *probability >= 0.0)
                && !points.contains(&probabilities)
            {
                points.push(probabilities);
            }
        }
    }
    Ok(points)
}

fn all_zero_minimum(
    strata_count: u32,
    margin: f64,
    cap: u32,
) -> Result<Option<u32>, StatisticsError> {
    let z_squared = ONE_SIDED_95_Z.powi(2);
    let total_required = (z_squared * (1.0 / margin - 1.0)).floor() + 1.0;
    let repetitions = positive_f64_as_u32((total_required / f64::from(strata_count)).ceil());
    Ok((repetitions <= cap).then_some(repetitions.max(1)))
}

fn binary_power(
    config: &PlannerConfig,
    endpoint: &str,
    margin: f64,
    repetitions: u32,
    nuisance: &[[f64; 4]],
    seed: &str,
) -> Result<(f64, f64), StatisticsError> {
    let total_pairs = 16.0 * f64::from(repetitions);
    let worst_discordance = nuisance
        .iter()
        .map(|point| point[1] + point[2])
        .fold(0.0_f64, f64::max);
    let standard_error = (worst_discordance / total_pairs).sqrt();
    let analytic_power = if standard_error == 0.0 {
        1.0
    } else {
        normal_cdf(margin / standard_error - ONE_SIDED_95_Z)
    };
    monte_carlo_power_bound(config, endpoint, analytic_power, seed)
}

#[cfg(test)]
fn plan_efficiency_alternative(
    config: &PlannerConfig,
    pilot: &BlindedPlannerInput,
    cost_improves: bool,
    seed: &str,
) -> Result<EndpointSampleRequirement, StatisticsError> {
    let alternatives = plan_efficiency_alternatives(config, pilot, seed)?;
    Ok(alternatives[usize::from(!cost_improves)].clone())
}

fn plan_efficiency_alternatives(
    config: &PlannerConfig,
    pilot: &BlindedPlannerInput,
    seed: &str,
) -> Result<[EndpointSampleRequirement; 2], StatisticsError> {
    let cost_variance = conservative_efficiency_variance(pilot, true)?;
    let response_variance = conservative_efficiency_variance(pilot, false)?;
    let curves =
        simulated_efficiency_power_curves(config, pilot, cost_variance, response_variance, seed)?;
    Ok([
        efficiency_requirement(config, "efficiency_cost_improves", &curves[0]),
        efficiency_requirement(config, "efficiency_responses_improve", &curves[1]),
    ])
}

fn efficiency_requirement(
    config: &PlannerConfig,
    endpoint: &str,
    curve: &[(f64, f64)],
) -> EndpointSampleRequirement {
    let required = (2..=config.maximum_pairs_per_archetype)
        .find(|pairs| curve[usize::try_from(*pairs - 1).unwrap()].1 >= config.target_power);
    let measured = required.unwrap_or(config.maximum_pairs_per_archetype);
    let (achieved, achieved_lower) = curve[usize::try_from(measured - 1).unwrap()];
    EndpointSampleRequirement {
        endpoint: endpoint.into(),
        required_pairs_per_archetype: required,
        achieved_power: achieved,
        achieved_power_lower_bound: achieved_lower,
        all_extreme_minimum: None,
        nuisance_points_evaluated: 2,
    }
}

fn simulated_efficiency_power_curves(
    config: &PlannerConfig,
    pilot: &BlindedPlannerInput,
    cost_variance: f64,
    response_variance: f64,
    seed: &str,
) -> Result<[Vec<(f64, f64)>; 2], StatisticsError> {
    let observations = pilot
        .strata
        .iter()
        .flat_map(|stratum| &stratum.efficiency)
        .flat_map(|pair| [pair.first_responses, pair.second_responses])
        .collect::<Vec<_>>();
    let (response_improvement_feasible, retention) = response_improvement_retention(&observations);
    let curve_len = usize::try_from(config.maximum_pairs_per_archetype).unwrap();
    let mut successes = [vec![0_u64; curve_len], vec![0_u64; curve_len]];
    for simulation in 0..config.simulation_replicates {
        let simulation = simulation.to_be_bytes();
        let mut rng = CounterRng::new(
            b"planner-joint-efficiency-power-v4",
            &[seed.as_bytes(), &simulation],
        );
        let mut current_cost = 0.0;
        let mut compact_cost = 0.0;
        let mut current_total = 0_u64;
        let mut compact_total = 0_u64;
        let mut improved_compact_total = 0_u64;
        for repetitions in 1..=config.maximum_pairs_per_archetype {
            for stratum in &pilot.strata {
                let choices = u64::try_from(stratum.efficiency.len())
                    .unwrap()
                    .saturating_mul(2);
                let (choice, thinning_draw) = choice_and_unit(choices, &mut rng);
                let selected = usize::try_from(choice / 2).unwrap();
                let pair = stratum.efficiency[selected];
                let (current_pair_cost, compact_pair_cost, current, compact) = if choice % 2 == 1 {
                    (
                        pair.first_cost,
                        pair.second_cost,
                        pair.first_responses,
                        pair.second_responses,
                    )
                } else {
                    (
                        pair.second_cost,
                        pair.first_cost,
                        pair.second_responses,
                        pair.first_responses,
                    )
                };
                current_cost += current_pair_cost;
                compact_cost += compact_pair_cost;
                current_total = current_total.saturating_add(current);
                compact_total = compact_total.saturating_add(compact);
                improved_compact_total = improved_compact_total.saturating_add(
                    thin_followup_total_with_unit(compact, 1, retention, thinning_draw),
                );
            }
            let repetitions_f64 = f64::from(repetitions);
            let cost_error = (cost_variance / (16.0 * repetitions_f64)).sqrt();
            let response_error = (response_variance / (16.0 * repetitions_f64)).sqrt();
            let cost_improvement = scalar_relative_change(current_cost, compact_cost * 0.9);
            let cost_unchanged = scalar_relative_change(current_cost, compact_cost);
            let response_unchanged =
                scalar_relative_change(u64_as_f64(current_total), u64_as_f64(compact_total));
            let response_improvement = scalar_relative_change(
                u64_as_f64(current_total),
                u64_as_f64(improved_compact_total),
            );
            let index = usize::try_from(repetitions - 1).unwrap();
            successes[0][index] += u64::from(
                efficiency_condition(cost_improvement, cost_error, true, config)
                    && efficiency_condition(response_unchanged, response_error, false, config),
            );
            successes[1][index] += u64::from(
                response_improvement_feasible
                    && efficiency_condition(cost_unchanged, cost_error, false, config)
                    && efficiency_condition(response_improvement, response_error, true, config),
            );
        }
    }
    let trials = u64::from(config.simulation_replicates);
    let mut curves = [Vec::with_capacity(curve_len), Vec::with_capacity(curve_len)];
    for alternative in 0..2 {
        for value in &successes[alternative] {
            let bounds = wilson_bounds(*value, trials, config.power_confidence_z)?;
            curves[alternative].push((bounds.estimate, bounds.lower));
        }
    }
    Ok(curves)
}

fn response_improvement_retention(observations: &[u64]) -> (bool, f64) {
    let count = usize_as_f64(observations.len());
    let observed_mean = observations
        .iter()
        .map(|value| u64_as_f64(*value))
        .sum::<f64>()
        / count;
    let positive_fraction =
        usize_as_f64(observations.iter().filter(|value| **value > 0).count()) / count;
    let target_mean = 0.9 * observed_mean;
    let feasible = target_mean + f64::EPSILON >= positive_fraction;
    let retention = if feasible && observed_mean > positive_fraction {
        ((target_mean - positive_fraction) / (observed_mean - positive_fraction)).clamp(0.0, 1.0)
    } else {
        1.0
    };
    (feasible, retention)
}

fn efficiency_condition(
    relative: f64,
    standard_error: f64,
    improves: bool,
    config: &PlannerConfig,
) -> bool {
    if improves {
        relative <= -config.worthwhile_efficiency_improvement
            && relative + 1.959_963_984_540_054 * standard_error < 0.0
    } else {
        relative + ONE_SIDED_95_Z * standard_error < config.efficiency_non_degradation_margin
    }
}

#[cfg(test)]
fn thin_followup_total(count: u64, repetitions: u32, retention: f64, rng: &mut CounterRng) -> u64 {
    thin_followup_total_with_unit(count, repetitions, retention, unit_f64(rng))
}

fn thin_followup_total_with_unit(count: u64, repetitions: u32, retention: f64, draw: f64) -> u64 {
    let repetitions = u64::from(repetitions);
    let followups = count.saturating_sub(1).saturating_mul(repetitions);
    let mandatory = u64::from(count > 0).saturating_mul(repetitions);
    mandatory.saturating_add(binomial_count(followups, retention, draw))
}

fn binomial_count(trials: u64, probability: f64, target: f64) -> u64 {
    if trials == 0 || probability <= 0.0 {
        return 0;
    }
    if probability >= 1.0 {
        return trials;
    }
    let odds = probability / (1.0 - probability);
    let mut probability_mass = (1.0 - probability).powf(u64_as_f64(trials));
    let mut cumulative = probability_mass;
    let mut successes = 0;
    while target > cumulative && successes < trials {
        probability_mass *= u64_as_f64(trials - successes) / u64_as_f64(successes + 1) * odds;
        successes += 1;
        cumulative += probability_mass;
    }
    successes
}

fn choice_and_unit(upper: u64, rng: &mut CounterRng) -> (u64, f64) {
    if upper > 256 {
        return (rng.bounded(upper), unit_f64(rng));
    }
    debug_assert!(upper > 0);
    let zone = 256 - 256 % upper;
    loop {
        let draw = rng.next_u64();
        let choice = draw >> 56;
        if choice < zone {
            let unit = u64_as_f64(draw & ((1_u64 << 56) - 1)) / 72_057_594_037_927_936.0;
            return (choice % upper, unit);
        }
    }
}

fn conservative_efficiency_variance(
    pilot: &BlindedPlannerInput,
    cost: bool,
) -> Result<f64, StatisticsError> {
    let values = pilot
        .strata
        .iter()
        .flat_map(|stratum| &stratum.efficiency)
        .map(|pair| {
            let (first, second) = if cost {
                (pair.first_cost, pair.second_cost)
            } else {
                (
                    u64_as_f64(pair.first_responses),
                    u64_as_f64(pair.second_responses),
                )
            };
            symmetric_relative_change(first, second)
        })
        .collect::<Vec<_>>();
    let mean = values.iter().sum::<f64>() / usize_as_f64(values.len());
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / usize_as_f64(values.len().saturating_sub(1));
    let degrees = usize_as_f64(values.len().saturating_sub(1));
    let chi_square_lower = degrees
        * (1.0 - 2.0 / (9.0 * degrees) - ONE_SIDED_95_Z * (2.0 / (9.0 * degrees)).sqrt())
            .max(0.01)
            .powi(3);
    Ok((degrees * variance / chi_square_lower).max(1e-12))
}

fn scalar_relative_change(current: f64, compact: f64) -> f64 {
    if current == 0.0 {
        if compact == 0.0 { 0.0 } else { f64::INFINITY }
    } else {
        compact / current - 1.0
    }
}

fn symmetric_relative_change(first: f64, second: f64) -> f64 {
    let mean = (first + second) / 2.0;
    if mean == 0.0 {
        0.0
    } else {
        (second - first) / mean
    }
}

fn monte_carlo_power_bound(
    config: &PlannerConfig,
    endpoint: &str,
    analytic_power: f64,
    seed: &str,
) -> Result<(f64, f64), StatisticsError> {
    let mut rng = CounterRng::new(
        b"planner-common-power-draws-v2",
        &[seed.as_bytes(), endpoint.as_bytes()],
    );
    let successes = (0..config.simulation_replicates)
        .filter(|_| unit_f64(&mut rng) < analytic_power)
        .count();
    let successes = u64::try_from(successes).unwrap();
    let trials = u64::from(config.simulation_replicates);
    let bounds = wilson_bounds(successes, trials, config.power_confidence_z)?;
    Ok((bounds.estimate, bounds.lower))
}

fn normal_cdf(value: f64) -> f64 {
    let sign = if value < 0.0 { -1.0 } else { 1.0 };
    let x = value.abs() / 2.0_f64.sqrt();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let polynomial =
        (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t;
    let erf = sign * (1.0 - polynomial * (-x * x).exp());
    0.5 * (1.0 + erf)
}

fn find_required_sample(
    config: &PlannerConfig,
    minimum: u32,
    mut power: impl FnMut(u32) -> Result<(f64, f64), StatisticsError>,
) -> Result<(Option<u32>, f64, f64), StatisticsError> {
    let cap = config.maximum_pairs_per_archetype;
    if minimum > cap {
        return Ok((None, 0.0, 0.0));
    }
    let mut lower = minimum.saturating_sub(1);
    let mut upper = minimum;
    let (mut upper_power, mut upper_lower) = power(upper)?;
    while upper_lower < config.target_power && upper < cap {
        lower = upper;
        upper = upper.saturating_mul(2).min(cap);
        (upper_power, upper_lower) = power(upper)?;
    }
    if upper_lower < config.target_power {
        return Ok((None, upper_power, upper_lower));
    }
    while upper.saturating_sub(lower) > 1 {
        let candidate = lower + (upper - lower) / 2;
        let (candidate_power, candidate_lower) = power(candidate)?;
        if candidate_lower >= config.target_power {
            upper = candidate;
            upper_power = candidate_power;
            upper_lower = candidate_lower;
        } else {
            lower = candidate;
        }
    }
    Ok((Some(upper), upper_power, upper_lower))
}

fn unit_f64(rng: &mut CounterRng) -> f64 {
    u64_as_f64(rng.next_u64()) / (u64_as_f64(u64::MAX) + 1.0)
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
fn positive_f64_as_usize(value: f64) -> usize {
    debug_assert!(value.is_finite() && value >= 0.0 && value <= usize::MAX as f64);
    value as usize
}

#[allow(clippy::as_conversions)]
fn positive_f64_as_u32(value: f64) -> u32 {
    debug_assert!(value.is_finite() && value >= 0.0 && value <= f64::from(u32::MAX));
    value as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform_binary_strata(pair: BinaryPair, repetitions: usize) -> Vec<BinaryStratum> {
        (0..16)
            .map(|index| BinaryStratum {
                archetype_id: format!("a{index}"),
                weight: 1.0 / 16.0,
                pairs: vec![pair; repetitions],
            })
            .collect()
    }

    fn stratum_from_counts(
        archetype_id: &str,
        weight: f64,
        n00: usize,
        n01: usize,
        n10: usize,
        n11: usize,
    ) -> BinaryStratum {
        let mut pairs = Vec::with_capacity(n00 + n01 + n10 + n11);
        pairs.extend(vec![
            BinaryPair {
                current: false,
                compact: false,
            };
            n00
        ]);
        pairs.extend(vec![
            BinaryPair {
                current: false,
                compact: true,
            };
            n01
        ]);
        pairs.extend(vec![
            BinaryPair {
                current: true,
                compact: false,
            };
            n10
        ]);
        pairs.extend(vec![
            BinaryPair {
                current: true,
                compact: true,
            };
            n11
        ]);
        BinaryStratum {
            archetype_id: archetype_id.into(),
            weight,
            pairs,
        }
    }

    #[test]
    fn all_zero_and_all_pass_match_score_formula() {
        let repetitions = 25;
        let total_pairs = 16.0 * usize_as_f64(repetitions);
        let expected = ONE_SIDED_95_Z.powi(2) / (total_pairs + ONE_SIDED_95_Z.powi(2));
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
            let strata = uniform_binary_strata(pair, repetitions);
            let bounds = paired_risk_difference_bounds(&strata, 0.05).unwrap();
            assert_eq!(bounds.estimate, 0.0);
            assert!((bounds.lower + expected).abs() < 2e-10);
            assert!((bounds.upper - expected).abs() < 2e-10);
            assert!(paired_upper_bound_below(&strata, expected * 1.01, 0.05).unwrap());
            assert!(!paired_upper_bound_below(&strata, expected * 0.99, 0.05).unwrap());
        }
    }

    #[test]
    fn variant_swap_is_symmetric() {
        let original_strata = vec![
            stratum_from_counts("heavy", 0.7, 8, 5, 2, 5),
            stratum_from_counts("light", 0.3, 4, 1, 7, 8),
        ];
        let original = paired_risk_difference_bounds(&original_strata, 0.05).unwrap();
        let swapped = paired_risk_difference_bounds(
            &original_strata
                .iter()
                .map(|stratum| BinaryStratum {
                    archetype_id: stratum.archetype_id.clone(),
                    weight: stratum.weight,
                    pairs: stratum
                        .pairs
                        .iter()
                        .map(|pair| BinaryPair {
                            current: pair.compact,
                            compact: pair.current,
                        })
                        .collect(),
                })
                .collect::<Vec<_>>(),
            0.05,
        )
        .unwrap();
        assert!((original.estimate + swapped.estimate).abs() < 1e-12);
        assert!((original.lower + swapped.upper).abs() < 2e-9);
        assert!((original.upper + swapped.lower).abs() < 2e-9);
    }

    #[test]
    fn event_complement_is_symmetric() {
        let original_strata = vec![
            stratum_from_counts("heavy", 0.7, 8, 5, 2, 5),
            stratum_from_counts("light", 0.3, 4, 1, 7, 8),
        ];
        let original = paired_risk_difference_bounds(&original_strata, 0.05).unwrap();
        let complemented = paired_risk_difference_bounds(
            &original_strata
                .iter()
                .map(|stratum| BinaryStratum {
                    archetype_id: stratum.archetype_id.clone(),
                    weight: stratum.weight,
                    pairs: stratum
                        .pairs
                        .iter()
                        .map(|pair| BinaryPair {
                            current: !pair.current,
                            compact: !pair.compact,
                        })
                        .collect(),
                })
                .collect::<Vec<_>>(),
            0.05,
        )
        .unwrap();
        assert!((original.estimate + complemented.estimate).abs() < 1e-12);
        assert!((original.lower + complemented.upper).abs() < 2e-9);
        assert!((original.upper + complemented.lower).abs() < 2e-9);
    }

    #[test]
    fn balanced_discordance_matches_score_formula() {
        let n = 40.0;
        let expected = ONE_SIDED_95_Z / (n + ONE_SIDED_95_Z.powi(2)).sqrt();
        let bounds =
            paired_risk_difference_bounds(&[stratum_from_counts("one", 1.0, 0, 20, 20, 0)], 0.05)
                .unwrap();
        assert_eq!(bounds.estimate, 0.0);
        assert!((bounds.lower + expected).abs() < 2e-10);
        assert!((bounds.upper - expected).abs() < 2e-10);
    }

    #[test]
    fn single_stratum_matches_independent_profile_reference() {
        let bounds =
            paired_risk_difference_bounds(&[stratum_from_counts("one", 1.0, 12, 5, 2, 11)], 0.05)
                .unwrap();
        assert!((bounds.estimate - 0.1).abs() < 1e-12);
        assert!((bounds.lower - -0.051_179_683_200_790_124).abs() < 2e-9);
        assert!((bounds.upper - 0.255_231_209_265_059_04).abs() < 2e-9);
    }

    #[test]
    fn honors_fixed_weights_with_unequal_stratum_sizes() {
        let strata = vec![
            stratum_from_counts("heavy", 0.8, 0, 10, 0, 0),
            stratum_from_counts("light", 0.2, 0, 0, 100, 0),
        ];
        let bounds = paired_risk_difference_bounds(&strata, 0.05).unwrap();
        assert!((bounds.estimate - 0.6).abs() < 1e-12);
        assert!(bounds.lower < bounds.estimate);
        assert!(bounds.upper > bounds.estimate);
    }

    #[test]
    fn all_concordant_bounds_shrink_with_sample_size() {
        let pair = BinaryPair {
            current: false,
            compact: false,
        };
        let small = paired_risk_difference_bounds(&uniform_binary_strata(pair, 5), 0.05).unwrap();
        let large = paired_risk_difference_bounds(&uniform_binary_strata(pair, 50), 0.05).unwrap();
        assert!(large.upper < small.upper);
        assert!(large.lower > small.lower);
    }

    #[test]
    fn paired_profile_score_rejects_invalid_inputs() {
        assert!(paired_risk_difference_bounds(&[], 0.05).is_err());
        assert!(
            paired_risk_difference_bounds(
                &uniform_binary_strata(
                    BinaryPair {
                        current: false,
                        compact: false,
                    },
                    1
                ),
                0.1
            )
            .is_err()
        );
        assert!(
            paired_risk_difference_bounds(
                &[BinaryStratum {
                    archetype_id: "empty".into(),
                    weight: 1.0,
                    pairs: Vec::new(),
                }],
                0.05,
            )
            .is_err()
        );
        assert!(
            paired_risk_difference_bounds(
                &[
                    stratum_from_counts("one", 0.4, 1, 0, 0, 0),
                    stratum_from_counts("two", 0.4, 1, 0, 0, 0),
                ],
                0.05,
            )
            .is_err()
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
    fn all_zero_efficiency_is_explicitly_undefined() {
        let strata = (0..16)
            .map(|index| EfficiencyStratum {
                archetype_id: format!("a{index}"),
                weight: 1.0 / 16.0,
                pairs: vec![EfficiencyPair {
                    current: 0.0,
                    compact: 0.0,
                }],
            })
            .collect::<Vec<_>>();
        let summary = paired_relative_change_bootstrap(
            &strata,
            &BootstrapConfig {
                replicates: 10,
                seed: "zero".into(),
            },
        )
        .unwrap();
        assert!(!summary.defined);
        assert!(!summary.bounds_defined);
    }

    #[test]
    fn sparse_defined_endpoint_reports_unbounded_bootstrap() {
        let strata = (0..16)
            .map(|index| EfficiencyStratum {
                archetype_id: format!("a{index}"),
                weight: 1.0 / 16.0,
                pairs: if index == 0 {
                    vec![
                        EfficiencyPair {
                            current: 1.0,
                            compact: 1.0,
                        },
                        EfficiencyPair {
                            current: 0.0,
                            compact: 0.0,
                        },
                    ]
                } else {
                    vec![
                        EfficiencyPair {
                            current: 0.0,
                            compact: 0.0,
                        };
                        2
                    ]
                },
            })
            .collect::<Vec<_>>();
        let summary = paired_relative_change_bootstrap(
            &strata,
            &BootstrapConfig {
                replicates: 100,
                seed: "sparse".into(),
            },
        )
        .unwrap();
        assert!(summary.defined);
        assert!(!summary.bounds_defined);
        assert_eq!(summary.relative_change, 0.0);
    }

    #[test]
    fn wilson_never_collapses_at_extremes() {
        let zero = wilson_bounds(0, 20, 1.96).unwrap();
        let all = wilson_bounds(20, 20, 1.96).unwrap();
        assert!(zero.upper > 0.0);
        assert!(all.lower < 1.0);
        assert!((zero.upper + all.lower - 1.0).abs() < 1e-12);
    }

    fn planner_input(counts: PairedEventCounts, spread: f64) -> BlindedPlannerInput {
        BlindedPlannerInput {
            strata: (0..16)
                .map(|index| BlindedPlannerStratum {
                    archetype_id: format!("a{index}"),
                    task_failure: counts,
                    patch_failure: counts,
                    edit_bypass: counts,
                    efficiency: (0..usize::try_from(counts.pairs()).unwrap())
                        .map(|pair| {
                            let offset = if pair % 2 == 0 { spread } else { -spread };
                            BlindedEfficiencyPair {
                                first_cost: 10.0 + offset,
                                second_cost: 10.0 - offset,
                                first_responses: if offset.is_sign_positive() { 9 } else { 11 },
                                second_responses: if offset.is_sign_positive() { 11 } else { 9 },
                            }
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn test_planner_config(cap: u32) -> PlannerConfig {
        PlannerConfig {
            simulation_replicates: 512,
            maximum_pairs_per_archetype: cap,
            ..PlannerConfig::default()
        }
    }

    #[test]
    fn planner_handles_zero_events_deterministically() {
        let pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.1,
        );
        let config = test_planner_config(1);
        let first = plan_sample(&config, &pilot, "planner").unwrap();
        let second = plan_sample(&config, &pilot, "planner").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.conclusion,
            PlanningConclusion::InconclusiveInsufficientUniverse
        );
        let nuisance =
            conservative_nuisance_points(event_counts(&pilot, |stratum| stratum.patch_failure))
                .unwrap();
        assert!(nuisance.iter().any(|point| point[1] + point[2] > 0.0));
        assert_eq!(all_zero_minimum(16, 0.02, 20).unwrap(), Some(9));
    }

    #[test]
    fn planner_requires_the_frozen_simulation_count() {
        let pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.1,
        );
        let config = PlannerConfig {
            simulation_replicates: 511,
            maximum_pairs_per_archetype: 1,
            ..PlannerConfig::default()
        };
        assert!(plan_sample(&config, &pilot, "replicate-contract").is_err());

        let malformed = planner_input(
            PairedEventCounts {
                neither: 129,
                ..PairedEventCounts::default()
            },
            0.1,
        );
        assert!(plan_sample(&PlannerConfig::default(), &malformed, "oversized-pilot").is_err());
    }

    #[test]
    fn response_thinning_preserves_zero_and_uses_exact_extremes() {
        let mut rng = CounterRng::new(b"thinning-test", &[]);
        assert_eq!(thin_followup_total(0, 4, 0.5, &mut rng), 0);
        assert_eq!(thin_followup_total(5, 3, 0.0, &mut rng), 3);
        assert_eq!(thin_followup_total(5, 3, 1.0, &mut rng), 15);

        let (feasible, retention) = response_improvement_retention(&[0, 4]);
        assert!(feasible);
        assert!((0.5 + retention * 1.5 - 1.8).abs() < 1e-12);
        assert!(!response_improvement_retention(&[0, 1]).0);
    }

    #[test]
    fn planner_accepts_zero_response_timeout_observations() {
        let mut pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.0,
        );
        for stratum in &mut pilot.strata {
            for pair in &mut stratum.efficiency {
                pair.first_cost = 0.0;
                pair.second_cost = 0.0;
                pair.first_responses = 0;
                pair.second_responses = 0;
            }
        }
        let plan = plan_sample(
            &PlannerConfig {
                maximum_pairs_per_archetype: 1,
                ..PlannerConfig::default()
            },
            &pilot,
            "zero-response",
        )
        .unwrap();
        assert_eq!(
            plan.conclusion,
            PlanningConclusion::InconclusiveInsufficientUniverse
        );
    }

    #[test]
    fn planner_is_orientation_invariant_and_detects_insufficient_universe() {
        let mut pilot = planner_input(
            PairedEventCounts {
                neither: 1,
                first_only: 1,
                second_only: 1,
                ..PairedEventCounts::default()
            },
            1.0,
        );
        let mut swapped = pilot.clone();
        for stratum in &mut swapped.strata {
            std::mem::swap(
                &mut stratum.task_failure.first_only,
                &mut stratum.task_failure.second_only,
            );
            std::mem::swap(
                &mut stratum.patch_failure.first_only,
                &mut stratum.patch_failure.second_only,
            );
            std::mem::swap(
                &mut stratum.edit_bypass.first_only,
                &mut stratum.edit_bypass.second_only,
            );
            for pair in &mut stratum.efficiency {
                std::mem::swap(&mut pair.first_cost, &mut pair.second_cost);
                std::mem::swap(&mut pair.first_responses, &mut pair.second_responses);
            }
        }
        let config = test_planner_config(1);
        let original = plan_sample(&config, &pilot, "orientation").unwrap();
        let reversed = plan_sample(&config, &swapped, "orientation").unwrap();
        assert_eq!(original, reversed);
        assert_eq!(
            original.conclusion,
            PlanningConclusion::InconclusiveInsufficientUniverse
        );
        pilot.strata[0].task_failure = PairedEventCounts::default();
    }

    #[test]
    fn high_discordance_requires_no_less_than_zero_event_pilot() {
        let zero = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.1,
        );
        let discordant = planner_input(
            PairedEventCounts {
                first_only: 2,
                second_only: 1,
                ..PairedEventCounts::default()
            },
            0.1,
        );
        let config = test_planner_config(20);
        let zero_plan = plan_binary_endpoint(
            &config,
            "patch",
            0.03,
            event_counts(&zero, |stratum| stratum.patch_failure),
            "high-discordance",
        )
        .unwrap();
        let discordant_plan = plan_binary_endpoint(
            &config,
            "patch",
            0.03,
            event_counts(&discordant, |stratum| stratum.patch_failure),
            "high-discordance",
        )
        .unwrap();
        assert!(
            discordant_plan
                .required_pairs_per_archetype
                .unwrap_or(u32::MAX)
                >= zero_plan.required_pairs_per_archetype.unwrap_or(0)
        );
    }

    #[test]
    fn returned_endpoint_power_meets_contract_when_recommended() {
        let pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.01,
        );
        let config = test_planner_config(20);
        let endpoint = plan_binary_endpoint(
            &config,
            "edit_bypass",
            0.02,
            event_counts(&pilot, |stratum| stratum.edit_bypass),
            "power-contract",
        )
        .unwrap();
        if endpoint.required_pairs_per_archetype.is_some() {
            assert!(endpoint.achieved_power >= config.target_power);
        }
    }

    #[test]
    fn efficiency_variance_can_limit_the_sample() {
        let mut pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.0,
        );
        for stratum in &mut pilot.strata {
            for (index, pair) in stratum.efficiency.iter_mut().enumerate() {
                pair.first_cost = 10.0;
                pair.second_cost = 10.0;
                pair.first_responses = if index % 2 == 0 { 4 } else { 16 };
                pair.second_responses = if index % 2 == 0 { 16 } else { 4 };
            }
        }
        let config = PlannerConfig {
            simulation_replicates: 4,
            maximum_pairs_per_archetype: 30,
            ..PlannerConfig::default()
        };
        let cost = plan_efficiency_alternative(&config, &pilot, true, "efficiency-limit").unwrap();
        let responses =
            plan_efficiency_alternative(&config, &pilot, false, "efficiency-limit").unwrap();
        assert!(
            responses.required_pairs_per_archetype.unwrap_or(u32::MAX)
                >= cost.required_pairs_per_archetype.unwrap_or(0)
        );
    }

    #[test]
    fn joint_efficiency_power_does_not_treat_marginals_as_joint() {
        let pilot = BlindedPlannerInput {
            strata: (0..16)
                .map(|index| BlindedPlannerStratum {
                    archetype_id: format!("a{index}"),
                    task_failure: PairedEventCounts {
                        neither: 1,
                        ..PairedEventCounts::default()
                    },
                    patch_failure: PairedEventCounts {
                        neither: 1,
                        ..PairedEventCounts::default()
                    },
                    edit_bypass: PairedEventCounts {
                        neither: 1,
                        ..PairedEventCounts::default()
                    },
                    efficiency: vec![BlindedEfficiencyPair {
                        first_cost: 20.0,
                        second_cost: 10.0,
                        first_responses: 10,
                        second_responses: 20,
                    }],
                })
                .collect(),
        };
        let config = PlannerConfig {
            maximum_pairs_per_archetype: 1,
            ..PlannerConfig::default()
        };
        let curves = simulated_efficiency_power_curves(
            &config,
            &pilot,
            conservative_efficiency_variance(&pilot, true).unwrap(),
            conservative_efficiency_variance(&pilot, false).unwrap(),
            "joint-condition",
        )
        .unwrap();
        assert!(curves[0][0].0 < 0.25, "joint curve: {:?}", curves[0][0]);
    }

    #[test]
    fn response_simulation_averages_independent_asymmetric_pairs() {
        let mut pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.0,
        );
        for stratum in &mut pilot.strata {
            for (index, pair) in stratum.efficiency.iter_mut().enumerate() {
                pair.first_responses = if index % 2 == 0 { 4 } else { 16 };
                pair.second_responses = if index % 2 == 0 { 16 } else { 4 };
            }
        }
        let config = PlannerConfig {
            simulation_replicates: 512,
            maximum_pairs_per_archetype: 64,
            ..PlannerConfig::default()
        };
        let response_variance = conservative_efficiency_variance(&pilot, false).unwrap();
        let cost_variance = conservative_efficiency_variance(&pilot, true).unwrap();
        let curves = simulated_efficiency_power_curves(
            &config,
            &pilot,
            cost_variance,
            response_variance,
            "independent-asymmetric",
        )
        .unwrap();
        let curve = &curves[1];
        let small = curve[1];
        let large = curve[63];
        assert!(large.1 > small.1, "small={small:?}, large={large:?}");
    }

    #[test]
    fn response_observations_are_integer_and_low_variance_is_not_underpowered() {
        let fractional = serde_json::json!({
            "first_cost": 1.0,
            "second_cost": 1.0,
            "first_responses": 1.5,
            "second_responses": 2
        });
        assert!(serde_json::from_value::<BlindedEfficiencyPair>(fractional).is_err());

        let pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.0,
        );
        let config = PlannerConfig {
            simulation_replicates: 512,
            maximum_pairs_per_archetype: 80,
            ..PlannerConfig::default()
        };
        let requirement =
            plan_efficiency_alternative(&config, &pilot, false, "integer-responses").unwrap();
        assert!(requirement.required_pairs_per_archetype.is_some());
        assert!(requirement.achieved_power_lower_bound >= config.target_power);
    }

    #[test]
    #[ignore = "benchmark-style planner runtime bound"]
    fn representative_planner_completes_within_ten_seconds() {
        let pilot = planner_input(
            PairedEventCounts {
                neither: 3,
                ..PairedEventCounts::default()
            },
            0.1,
        );
        let started = std::time::Instant::now();
        let _ = plan_sample(&PlannerConfig::default(), &pilot, "runtime-bound").unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
