//! iDesign network-health metrics.
//!
//! Provides two pure functions for assessing a project's structural health:
//!
//! - [`cyclomatic_complexity`] — McCabe-style C = deps − activities + 2, with
//!   three qualitative bands ([`CcFlag`]).
//! - [`project_efficiency`] — planned / actual effort ratio with three
//!   qualitative bands ([`EffFlag`]), guarded against division-by-zero.

// ── Cyclomatic complexity ──────────────────────────────────────────────────

/// Qualitative band for the cyclomatic-complexity score.
///
/// | Band        | C range        |
/// |-------------|----------------|
/// | `InTarget`  | C ≤ 12         |
/// | `Warn`      | 13 ≤ C ≤ 15    |
/// | `TooComplex`| C > 15         |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcFlag {
    /// C ≤ 12 — within healthy bounds (includes under-connected networks).
    InTarget,
    /// 13 ≤ C ≤ 15 — approaching complexity limit; review recommended.
    Warn,
    /// C > 15 — network is too complex; restructuring required.
    TooComplex,
}

/// Compute the cyclomatic complexity of a project network and classify it.
///
/// The formula is the standard McCabe / network formula:
/// ```text
/// C = num_dependencies − num_activities + 2
/// ```
///
/// # Arguments
/// * `num_dependencies` — number of dependency edges in the network.
/// * `num_activities`   — number of activity nodes in the network.
///
/// # Returns
/// `(C, flag)` where `flag` is:
/// * [`CcFlag::InTarget`] if C ≤ 12,
/// * [`CcFlag::Warn`] if 13 ≤ C ≤ 15,
/// * [`CcFlag::TooComplex`] if C > 15.
pub fn cyclomatic_complexity(num_dependencies: usize, num_activities: usize) -> (i64, CcFlag) {
    let c = num_dependencies as i64 - num_activities as i64 + 2;
    let flag = if c <= 12 {
        CcFlag::InTarget
    } else if c <= 15 {
        CcFlag::Warn
    } else {
        CcFlag::TooComplex
    };
    (c, flag)
}

// ── Project efficiency ─────────────────────────────────────────────────────

/// Qualitative band for the project-efficiency ratio.
///
/// Target band is 0.15 – 0.25 (inclusive on both ends).
///
/// | Band       | Ratio            |
/// |------------|------------------|
/// | `Low`      | ratio < 0.15     |
/// | `InTarget` | 0.15 ≤ ratio ≤ 0.25 |
/// | `High`     | ratio > 0.25     |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffFlag {
    /// Ratio < 0.15 — actual effort significantly exceeds planned effort.
    Low,
    /// 0.15 ≤ ratio ≤ 0.25 — healthy efficiency.
    InTarget,
    /// Ratio > 0.25 — planned effort significantly exceeds actual effort
    /// (under-delivery or scope creep on planned side).
    High,
}

/// Compute the project efficiency as `planned / actual` and classify it.
///
/// Returns `None` when `sum_actual_effort == 0` to avoid division by zero.
///
/// # Arguments
/// * `sum_planned_effort` — total planned effort tokens across all activities.
/// * `sum_actual_effort`  — total actual effort tokens across all activities.
///
/// # Returns
/// * `None` if `sum_actual_effort == 0`.
/// * `Some((ratio, flag))` otherwise, where `ratio = planned / actual` and
///   `flag` classifies the ratio against the 0.15–0.25 target band.
pub fn project_efficiency(
    sum_planned_effort: u64,
    sum_actual_effort: u64,
) -> Option<(f64, EffFlag)> {
    if sum_actual_effort == 0 {
        return None;
    }
    let ratio = sum_planned_effort as f64 / sum_actual_effort as f64;
    let flag = if ratio < 0.15 {
        EffFlag::Low
    } else if ratio <= 0.25 {
        EffFlag::InTarget
    } else {
        EffFlag::High
    };
    Some((ratio, flag))
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── cyclomatic_complexity ──────────────────────────────────────────────

    /// C = 12 → InTarget (upper boundary of the InTarget band).
    #[test]
    fn cc_boundary_12_is_in_target() {
        // deps=10, acts=0 → C=12; or more naturally: deps=20, acts=10 → C=12
        let (c, flag) = cyclomatic_complexity(20, 10);
        assert_eq!(c, 12);
        assert_eq!(flag, CcFlag::InTarget);
    }

    /// C = 13 → Warn (lower boundary of the Warn band).
    #[test]
    fn cc_boundary_13_is_warn() {
        // deps=21, acts=10 → C=13
        let (c, flag) = cyclomatic_complexity(21, 10);
        assert_eq!(c, 13);
        assert_eq!(flag, CcFlag::Warn);
    }

    /// C = 15 → Warn (upper boundary of the Warn band).
    #[test]
    fn cc_boundary_15_is_warn() {
        // deps=23, acts=10 → C=15
        let (c, flag) = cyclomatic_complexity(23, 10);
        assert_eq!(c, 15);
        assert_eq!(flag, CcFlag::Warn);
    }

    /// C = 16 → TooComplex (lower boundary of the TooComplex band).
    #[test]
    fn cc_boundary_16_is_too_complex() {
        // deps=24, acts=10 → C=16
        let (c, flag) = cyclomatic_complexity(24, 10);
        assert_eq!(c, 16);
        assert_eq!(flag, CcFlag::TooComplex);
    }

    /// Trivially small network (more activities than deps) → negative C;
    /// negative values are well within InTarget.
    #[test]
    fn cc_small_network_in_target() {
        let (c, flag) = cyclomatic_complexity(2, 5);
        assert_eq!(c, -1);
        assert_eq!(flag, CcFlag::InTarget);
    }

    // ── project_efficiency ─────────────────────────────────────────────────

    /// Guard: actual == 0 must return None (no division by zero).
    #[test]
    fn efficiency_none_when_actual_is_zero() {
        assert_eq!(project_efficiency(100, 0), None);
        assert_eq!(project_efficiency(0, 0), None);
    }

    /// planned == actual → ratio 1.0 → High (above 0.25).
    #[test]
    fn efficiency_equal_planned_actual_is_high() {
        let (ratio, flag) = project_efficiency(100, 100).expect("Some expected");
        assert!((ratio - 1.0).abs() < 1e-9);
        assert_eq!(flag, EffFlag::High);
    }

    /// ratio exactly 0.15 → InTarget (lower boundary, inclusive).
    #[test]
    fn efficiency_boundary_015_is_in_target() {
        // planned=15, actual=100 → ratio=0.15
        let (ratio, flag) = project_efficiency(15, 100).expect("Some expected");
        assert!((ratio - 0.15).abs() < 1e-9);
        assert_eq!(flag, EffFlag::InTarget);
    }

    /// ratio exactly 0.25 → InTarget (upper boundary, inclusive).
    #[test]
    fn efficiency_boundary_025_is_in_target() {
        // planned=25, actual=100 → ratio=0.25
        let (ratio, flag) = project_efficiency(25, 100).expect("Some expected");
        assert!((ratio - 0.25).abs() < 1e-9);
        assert_eq!(flag, EffFlag::InTarget);
    }

    /// ratio just above 0.25 → High.
    #[test]
    fn efficiency_just_above_025_is_high() {
        // planned=26, actual=100 → ratio=0.26
        let (ratio, flag) = project_efficiency(26, 100).expect("Some expected");
        assert!((ratio - 0.26).abs() < 1e-9);
        assert_eq!(flag, EffFlag::High);
    }

    /// ratio just below 0.15 → Low.
    #[test]
    fn efficiency_just_below_015_is_low() {
        // planned=14, actual=100 → ratio=0.14
        let (ratio, flag) = project_efficiency(14, 100).expect("Some expected");
        assert!((ratio - 0.14).abs() < 1e-9);
        assert_eq!(flag, EffFlag::Low);
    }

    /// Planned effort of zero with non-zero actual → ratio=0.0 → Low.
    #[test]
    fn efficiency_zero_planned_is_low() {
        let (ratio, flag) = project_efficiency(0, 50).expect("Some expected");
        assert!((ratio - 0.0).abs() < 1e-9);
        assert_eq!(flag, EffFlag::Low);
    }
}
