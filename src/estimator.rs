//! Effort Estimation for CPM Tasks
//!
//! Provides domain-neutral effort estimation. Given a [`TaskKind`] and an
//! optional complexity hint, returns a base estimate clamped to the
//! configured min/max range.
//!
//! # Scope
//!
//! This is intentionally a *minimal* estimator. It maps a [`TaskKind`] to
//! a base hour count, optionally applies a complexity multiplier, and
//! clamps to a configured range. A richer estimator can layer on top of
//! [`EffortEstimator::estimate`] without changing this contract.

use crate::task::TaskKind;
use serde::{Deserialize, Serialize};

/// Effort estimation configuration.
///
/// Can be loaded from a config file under `[cpm.effort]`:
/// ```toml
/// [cpm.effort]
/// cycle_base_hours = 4.0
/// spec_base_hours = 8.0
/// custom_base_hours = 4.0
/// complexity_multiplier = 1.5
/// min_hours = 0.5
/// max_hours = 40.0
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimationConfig {
    /// Base hours for breaking a dependency cycle.
    #[serde(default = "default_cycle_base")]
    pub cycle_base_hours: f32,

    /// Base hours for implementing a spec requirement.
    #[serde(default = "default_spec_base")]
    pub spec_base_hours: f32,

    /// Base hours for a custom task.
    #[serde(default = "default_custom_base")]
    pub custom_base_hours: f32,

    /// Multiplier for high-complexity tasks (applied when caller signals
    /// the work is "complex"; see [`EffortEstimator::estimate`]).
    #[serde(default = "default_complexity_multiplier")]
    pub complexity_multiplier: f32,

    /// Multiplier applied to tasks on the critical path.
    #[serde(default = "default_critical_multiplier")]
    pub critical_multiplier: f32,

    /// Minimum effort for any task (floor).
    #[serde(default = "default_min_hours")]
    pub min_hours: f32,

    /// Maximum effort for any task (ceiling).
    #[serde(default = "default_max_hours")]
    pub max_hours: f32,
}

const fn default_cycle_base() -> f32 {
    4.0
}
const fn default_spec_base() -> f32 {
    8.0
}
const fn default_custom_base() -> f32 {
    4.0
}
const fn default_complexity_multiplier() -> f32 {
    1.5
}
const fn default_critical_multiplier() -> f32 {
    1.0
}
const fn default_min_hours() -> f32 {
    0.5
}
const fn default_max_hours() -> f32 {
    40.0
}

impl Default for EstimationConfig {
    fn default() -> Self {
        Self {
            cycle_base_hours: default_cycle_base(),
            spec_base_hours: default_spec_base(),
            custom_base_hours: default_custom_base(),
            complexity_multiplier: default_complexity_multiplier(),
            critical_multiplier: default_critical_multiplier(),
            min_hours: default_min_hours(),
            max_hours: default_max_hours(),
        }
    }
}

/// Effort estimator using simple `TaskKind` heuristics.
pub struct EffortEstimator {
    config: EstimationConfig,
}

impl EffortEstimator {
    /// Create a new estimator with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: EstimationConfig::default(),
        }
    }

    /// Create an estimator with custom configuration.
    #[must_use]
    pub const fn with_config(config: EstimationConfig) -> Self {
        Self { config }
    }

    /// Clamp estimate to configured min/max.
    fn clamp(&self, hours: f32) -> f32 {
        hours.clamp(self.config.min_hours, self.config.max_hours)
    }

    /// Estimate effort for a task by its kind.
    ///
    /// `is_complex` is a coarse caller-supplied hint: when `true`, the
    /// configured [`EstimationConfig::complexity_multiplier`] is applied
    /// before clamping.
    ///
    /// For [`TaskKind::BreakCycle`], the cycle length further scales the
    /// estimate: cycles of length > 3 add `0.25 * (len - 3)` to the base
    /// multiplier; length <= 2 reduces it slightly.
    #[must_use]
    pub fn estimate(&self, kind: &TaskKind, is_complex: bool) -> f32 {
        let mut hours = match kind {
            TaskKind::BreakCycle { cycle } => {
                let mut h = self.config.cycle_base_hours;
                let cycle_len = cycle.len();
                if cycle_len > 3 {
                    h *= (cycle_len as f32 - 3.0).mul_add(0.25, 1.0);
                } else if cycle_len <= 2 {
                    h *= 0.8;
                }
                h
            }
            TaskKind::ImplementSpec { .. } => self.config.spec_base_hours,
            TaskKind::Custom { .. } => self.config.custom_base_hours,
        };

        if is_complex {
            hours *= self.config.complexity_multiplier;
        }

        self.clamp(hours)
    }

    /// Apply the critical-path multiplier to a previously computed estimate
    /// and re-clamp. Convenience for callers that decorate already-estimated
    /// tasks once they know which ones lie on the critical path.
    #[must_use]
    pub fn apply_critical_multiplier(&self, hours: f32) -> f32 {
        self.clamp(hours * self.config.critical_multiplier)
    }

    /// Get the configuration.
    #[must_use]
    pub const fn config(&self) -> &EstimationConfig {
        &self.config
    }
}

impl Default for EffortEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_default_estimates_within_bounds() {
        let estimator = EffortEstimator::new();
        let h = estimator.estimate(
            &TaskKind::Custom {
                description: "x".into(),
            },
            false,
        );
        assert!(h >= estimator.config().min_hours);
        assert!(h <= estimator.config().max_hours);
    }

    #[test]
    fn test_complexity_multiplier() {
        let estimator = EffortEstimator::new();
        let simple = estimator.estimate(
            &TaskKind::ImplementSpec {
                spec_id: "s".into(),
            },
            false,
        );
        let complex = estimator.estimate(
            &TaskKind::ImplementSpec {
                spec_id: "s".into(),
            },
            true,
        );
        assert!(complex > simple);
    }

    #[test]
    fn test_cycle_length_scales_estimate() {
        let estimator = EffortEstimator::new();
        let short = estimator.estimate(
            &TaskKind::BreakCycle {
                cycle: vec!["a".into(), "b".into()],
            },
            false,
        );
        let long = estimator.estimate(
            &TaskKind::BreakCycle {
                cycle: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
            },
            false,
        );
        assert!(long > short);
    }

    #[test]
    fn test_custom_config_applies() {
        let config = EstimationConfig {
            custom_base_hours: 10.0,
            ..Default::default()
        };
        let estimator = EffortEstimator::with_config(config);
        let h = estimator.estimate(
            &TaskKind::Custom {
                description: "x".into(),
            },
            false,
        );
        // Default custom_base is 4.0; bumped to 10.0 here.
        assert!(h >= 9.0);
    }

    #[test]
    fn test_clamp_to_max() {
        let config = EstimationConfig {
            min_hours: 1.0,
            max_hours: 5.0,
            spec_base_hours: 100.0,
            ..Default::default()
        };
        let estimator = EffortEstimator::with_config(config);
        let h = estimator.estimate(
            &TaskKind::ImplementSpec {
                spec_id: "s".into(),
            },
            true,
        );
        assert!(h <= 5.0);
    }

    #[test]
    fn test_apply_critical_multiplier_uses_config() {
        let config = EstimationConfig {
            critical_multiplier: 1.5,
            ..Default::default()
        };
        let estimator = EffortEstimator::with_config(config);
        let bumped = estimator.apply_critical_multiplier(2.0);
        // 2.0 * 1.5 = 3.0, within default clamp range
        assert_eq!(bumped, 3.0);
    }
}
