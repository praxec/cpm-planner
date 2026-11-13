//! iDesign risk engine for CPM Planner.
//!
//! Provides activity-risk and criticality-risk calculations following the
//! iDesign methodology, with const weights and a 0.4-0.75 band flag.
//!
//! # Float units
//! All `float` parameters here are **token-unit integers** (i.e. the integer
//! version of `Task::float` after rounding or as supplied by the caller).
//! Using integers eliminates floating-point comparison edge cases in the
//! `CriticalityBand` classifier.

// ── Criticality weights ──────────────────────────────────────────────────────

/// Weight for a Critical-band task.
pub const W_CRITICAL: u64 = 4;
/// Weight for a High-band task.
pub const W_HIGH: u64 = 3;
/// Weight for a Medium-band task.
pub const W_MEDIUM: u64 = 2;
/// Weight for a Low-band task.
pub const W_LOW: u64 = 1;

// ── CriticalityBand ──────────────────────────────────────────────────────────

/// Band that a task's float value falls into relative to the project maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CriticalityBand {
    /// Float == 0: on the critical path.
    Critical,
    /// Float in the lower third of the max range.
    High,
    /// Float in the middle third of the max range.
    Medium,
    /// Float in the upper third of the max range.
    Low,
}

// ── ValidatedThresholdSet ────────────────────────────────────────────────────

/// A strictly-ordered pair of thresholds (high_end < medium_end) used by
/// [`classify`].
///
/// The smart constructor panics at construction time if the invariant is
/// violated, so callers that hold a `ValidatedThresholdSet` are guaranteed
/// strict ordering.
#[derive(Debug, Clone, Copy)]
pub struct ValidatedThresholdSet {
    /// Upper bound (inclusive) of the **High** band (as a ratio in `[0,1)`).
    high_end: f64,
    /// Upper bound (inclusive) of the **Medium** band (as a ratio in `[0,1)`).
    medium_end: f64,
}

impl ValidatedThresholdSet {
    /// Create a new threshold set.
    ///
    /// # Panics
    /// Panics if `high_end >= medium_end` or either value is outside `(0, 1)`.
    #[must_use]
    pub fn new(high_end: f64, medium_end: f64) -> Self {
        assert!(
            high_end > 0.0 && high_end < 1.0,
            "high_end must be in (0, 1), got {high_end}"
        );
        assert!(
            medium_end > 0.0 && medium_end < 1.0,
            "medium_end must be in (0, 1), got {medium_end}"
        );
        assert!(
            high_end < medium_end,
            "high_end ({high_end}) must be strictly less than medium_end ({medium_end})"
        );
        Self {
            high_end,
            medium_end,
        }
    }

    /// The default 1/3-2/3 thresholds described in the iDesign methodology.
    #[must_use]
    pub fn default_thirds() -> Self {
        Self {
            high_end: 1.0 / 3.0,
            medium_end: 2.0 / 3.0,
        }
    }
}

// ── classify ─────────────────────────────────────────────────────────────────

/// Classify a task's float value into a [`CriticalityBand`].
///
/// - `float == 0`          => [`CriticalityBand::Critical`]
/// - `max_float == 0`      => every non-zero float would be impossible;
///   caller should guard, but we return `Critical` for safety.
/// - ratio `<= thresholds.high_end`   => [`CriticalityBand::High`]
/// - ratio `<= thresholds.medium_end` => [`CriticalityBand::Medium`]
/// - otherwise             => [`CriticalityBand::Low`]
#[must_use]
pub fn classify(float: u64, max_float: u64) -> CriticalityBand {
    let thresholds = ValidatedThresholdSet::default_thirds();
    classify_with(float, max_float, &thresholds)
}

/// Like [`classify`] but with an explicit threshold set.
#[must_use]
pub fn classify_with(
    float: u64,
    max_float: u64,
    thresholds: &ValidatedThresholdSet,
) -> CriticalityBand {
    if float == 0 || max_float == 0 {
        return CriticalityBand::Critical;
    }
    let ratio = float as f64 / max_float as f64;
    if ratio <= thresholds.high_end {
        CriticalityBand::High
    } else if ratio <= thresholds.medium_end {
        CriticalityBand::Medium
    } else {
        CriticalityBand::Low
    }
}

// ── ActivityRiskResult ───────────────────────────────────────────────────────

/// Result of [`activity_risk`].
#[derive(Debug, Clone, PartialEq)]
pub enum ActivityRiskResult {
    /// Computed risk score in `[0, 1]`.
    Risk(f64),
    /// Returned when `max_float == 0` (all tasks are on the critical path).
    /// No division is performed.
    AllCritical,
}

// ── activity_risk ─────────────────────────────────────────────────────────────

/// Compute the iDesign **activity risk** for a set of task float values.
///
/// Formula (when `max_float > 0`):
/// ```text
/// activity_risk = 1.0 - sum(floats) / (max_float * len(floats))
/// ```
///
/// Returns [`ActivityRiskResult::AllCritical`] when `max_float == 0` to
/// prevent division by zero.
///
/// # Edge cases
/// - Empty slice with `max_float > 0`: `sum == 0`, `len == 0` would
///   produce NaN (0/0). We treat an empty schedule as fully critical and
///   return `AllCritical` regardless of `max_float`.
#[must_use]
pub fn activity_risk(floats: &[u64], max_float: u64) -> ActivityRiskResult {
    if max_float == 0 || floats.is_empty() {
        return ActivityRiskResult::AllCritical;
    }
    let sum: u64 = floats.iter().sum();
    let denominator = max_float * floats.len() as u64;
    ActivityRiskResult::Risk(1.0 - (sum as f64) / (denominator as f64))
}

// ── criticality_risk ──────────────────────────────────────────────────────────

/// Compute the iDesign **criticality risk** from a set of [`CriticalityBand`]s.
///
/// Formula:
/// ```text
/// criticality_risk = sum(W_band_i) / (W_CRITICAL * len(bands))
/// ```
///
/// Returns `1.0` for an empty slice (maximally critical; no information to
/// suggest otherwise).
#[must_use]
pub fn criticality_risk(bands: &[CriticalityBand]) -> f64 {
    if bands.is_empty() {
        return 1.0;
    }
    let sum: u64 = bands
        .iter()
        .map(|b| match b {
            CriticalityBand::Critical => W_CRITICAL,
            CriticalityBand::High => W_HIGH,
            CriticalityBand::Medium => W_MEDIUM,
            CriticalityBand::Low => W_LOW,
        })
        .sum();
    let max_possible = W_CRITICAL * bands.len() as u64;
    sum as f64 / max_possible as f64
}

// ── RiskBandFlag ─────────────────────────────────────────────────────────────

/// Categorical flag for a risk score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskBandFlag {
    /// Score in `[0.4, 0.75]`: healthy range.
    InTarget,
    /// Score `> 0.75`: too many high-criticality tasks.
    HighRisk,
    /// Score `< 0.4`: schedule is over-decomposed / too much slack.
    OverDecompressed,
}

// ── band_flag ─────────────────────────────────────────────────────────────────

/// Map a risk score to a [`RiskBandFlag`].
///
/// | Range        | Flag               |
/// |--------------|--------------------|
/// | `< 0.4`      | `OverDecompressed` |
/// | `0.4 - 0.75` | `InTarget`         |
/// | `> 0.75`     | `HighRisk`         |
#[must_use]
pub fn band_flag(risk: f64) -> RiskBandFlag {
    if risk < 0.4 {
        RiskBandFlag::OverDecompressed
    } else if risk > 0.75 {
        RiskBandFlag::HighRisk
    } else {
        RiskBandFlag::InTarget
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Weight constants ──────────────────────────────────────────────────────

    #[test]
    fn test_weights_are_exactly_4_3_2_1() {
        assert_eq!(W_CRITICAL, 4, "W_CRITICAL must be exactly 4");
        assert_eq!(W_HIGH, 3, "W_HIGH must be exactly 3");
        assert_eq!(W_MEDIUM, 2, "W_MEDIUM must be exactly 2");
        assert_eq!(W_LOW, 1, "W_LOW must be exactly 1");
    }

    // ── classify ──────────────────────────────────────────────────────────────

    #[test]
    fn test_classify_zero_float_is_critical() {
        assert_eq!(classify(0, 12), CriticalityBand::Critical);
    }

    #[test]
    fn test_classify_high_band() {
        // 3 / 12 = 0.25 <= 1/3
        assert_eq!(classify(3, 12), CriticalityBand::High);
        // 4 / 12 = 0.333... <= 1/3 (boundary inclusive)
        assert_eq!(classify(4, 12), CriticalityBand::High);
    }

    #[test]
    fn test_classify_medium_band() {
        // 5 / 12 = 0.4166 -- just above 1/3
        assert_eq!(classify(5, 12), CriticalityBand::Medium);
        // 8 / 12 = 0.6666... <= 2/3 (boundary inclusive)
        assert_eq!(classify(8, 12), CriticalityBand::Medium);
    }

    #[test]
    fn test_classify_low_band() {
        // 9 / 12 = 0.75 > 2/3
        assert_eq!(classify(9, 12), CriticalityBand::Low);
        // 12 / 12 = 1.0
        assert_eq!(classify(12, 12), CriticalityBand::Low);
    }

    #[test]
    fn test_classify_max_float_zero_returns_critical() {
        assert_eq!(classify(5, 0), CriticalityBand::Critical);
    }

    // ── ValidatedThresholdSet ─────────────────────────────────────────────────

    #[test]
    #[should_panic]
    fn test_threshold_inverted_panics() {
        let _ = ValidatedThresholdSet::new(0.7, 0.3);
    }

    #[test]
    #[should_panic]
    fn test_threshold_equal_panics() {
        let _ = ValidatedThresholdSet::new(0.5, 0.5);
    }

    // ── activity_risk ─────────────────────────────────────────────────────────

    #[test]
    fn test_activity_risk_all_critical_when_max_float_zero() {
        let floats = [0u64, 0, 0];
        assert_eq!(activity_risk(&floats, 0), ActivityRiskResult::AllCritical);
    }

    #[test]
    fn test_activity_risk_all_zero_floats_with_positive_max() {
        // sum = 0, denom = 12 * 3 = 36  => risk = 1 - 0/36 = 1.0
        let floats = [0u64, 0, 0];
        assert_eq!(activity_risk(&floats, 12), ActivityRiskResult::Risk(1.0));
    }

    #[test]
    fn test_activity_risk_known_distribution() {
        // floats: [0, 4, 8], max_float = 12
        // sum = 12, denom = 12 * 3 = 36
        // risk = 1 - 12/36 = 1 - 0.333... = 0.666...
        let floats = [0u64, 4, 8];
        let max_float = 12u64;
        let expected = 1.0 - 12.0_f64 / 36.0_f64;
        match activity_risk(&floats, max_float) {
            ActivityRiskResult::Risk(r) => {
                assert!((r - expected).abs() < 1e-10, "expected {expected}, got {r}");
            }
            ActivityRiskResult::AllCritical => panic!("expected Risk, got AllCritical"),
        }
    }

    #[test]
    fn test_activity_risk_empty_returns_all_critical() {
        assert_eq!(activity_risk(&[], 10), ActivityRiskResult::AllCritical);
    }

    // ── criticality_risk ──────────────────────────────────────────────────────

    #[test]
    fn test_criticality_risk_all_critical_is_one() {
        // All tasks on critical path => all bands == Critical
        // sum = 4*3 = 12, max = 4*3 = 12  => 1.0
        let bands = [
            CriticalityBand::Critical,
            CriticalityBand::Critical,
            CriticalityBand::Critical,
        ];
        let r = criticality_risk(&bands);
        assert!(
            (r - 1.0).abs() < 1e-10,
            "all-critical criticality_risk must be 1.0, got {r}"
        );
    }

    #[test]
    fn test_criticality_risk_empty_is_one() {
        let r = criticality_risk(&[]);
        assert!(
            (r - 1.0).abs() < 1e-10,
            "empty criticality_risk must be 1.0, got {r}"
        );
    }

    #[test]
    fn test_criticality_risk_known_distribution() {
        // bands: [Critical(4), High(3), Medium(2), Low(1)]
        // sum = 10, max = 4*4 = 16  => 10/16 = 0.625
        let bands = [
            CriticalityBand::Critical,
            CriticalityBand::High,
            CriticalityBand::Medium,
            CriticalityBand::Low,
        ];
        let expected = 10.0_f64 / 16.0_f64;
        let r = criticality_risk(&bands);
        assert!((r - expected).abs() < 1e-10, "expected {expected}, got {r}");
    }

    #[test]
    fn test_criticality_risk_all_low_is_one_quarter() {
        // sum = 1*4 = 4, max = 4*4 = 16  => 4/16 = 0.25
        let bands = [CriticalityBand::Low; 4];
        let r = criticality_risk(&bands);
        assert!(
            (r - 0.25).abs() < 1e-10,
            "all-low criticality_risk must be 0.25, got {r}"
        );
    }

    // ── band_flag ─────────────────────────────────────────────────────────────

    #[test]
    fn test_band_flag_below_0_4_is_over_decompressed() {
        assert_eq!(band_flag(0.0), RiskBandFlag::OverDecompressed);
        assert_eq!(band_flag(0.39), RiskBandFlag::OverDecompressed);
        // 0.4 is boundary -- should be InTarget
        assert_eq!(band_flag(0.4), RiskBandFlag::InTarget);
    }

    #[test]
    fn test_band_flag_in_target_range() {
        assert_eq!(band_flag(0.4), RiskBandFlag::InTarget);
        assert_eq!(band_flag(0.57), RiskBandFlag::InTarget);
        assert_eq!(band_flag(0.75), RiskBandFlag::InTarget);
    }

    #[test]
    fn test_band_flag_above_0_75_is_high_risk() {
        assert_eq!(band_flag(0.751), RiskBandFlag::HighRisk);
        assert_eq!(band_flag(1.0), RiskBandFlag::HighRisk);
        // 0.75 is boundary -- should be InTarget
        assert_eq!(band_flag(0.75), RiskBandFlag::InTarget);
    }

    // ── Integration: all-critical schedule ───────────────────────────────────

    #[test]
    fn test_all_critical_schedule_end_to_end() {
        // All floats == 0, max_float == 0 (pure critical path)
        let floats = [0u64, 0, 0, 0];
        let max_float = 0u64;

        // activity_risk guard
        assert_eq!(
            activity_risk(&floats, max_float),
            ActivityRiskResult::AllCritical
        );

        // classify all zeros
        for &f in &floats {
            assert_eq!(classify(f, max_float), CriticalityBand::Critical);
        }

        // criticality_risk of all-critical bands
        let bands: Vec<_> = floats.iter().map(|&f| classify(f, max_float)).collect();
        let cr = criticality_risk(&bands);
        assert!(
            (cr - 1.0).abs() < 1e-10,
            "all-critical criticality_risk must be 1.0, got {cr}"
        );
    }
}
