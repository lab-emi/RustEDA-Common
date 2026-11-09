// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! Charge-equivalent loads over the linearized NLDM output transition.
//!
//! A transition table describes the output, rather than the inaccessible Thevenin source.
//! Its full linearized transition is the charge averaging interval. Once that interval gives
//! an effective load, the finite source ramp is fitted separately to the lower/upper crossings.
//! All arithmetic is f64; the scalar solve has constant storage and explicit work bounds.
//! STA-REQ-0733, STA-REQ-0734.

use super::{GateResponse, Pi, Poles, Thresholds, fit_full_duration, ramp_voltage};
use crate::ComputeError;

/// Resource and numerical bounds for one scalar effective-load solve.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Maximum safeguarded scalar root iterations, at most 256.
    pub max_iterations: usize,
    /// Maximum NLDM callback evaluations, including validation and the final lookup.
    pub max_evaluations: usize,
    /// Residual-decreasing trials per Newton step before bounded bisection fallback.
    pub max_line_search: usize,
    /// Allowed constant scalar workspace, excluding caller-owned immutable tables.
    pub max_aux_bytes: usize,
    /// Maximum reciprocal normalized local derivative of the charge residual.
    pub max_condition: f64,
    /// Final estimated capacitance error relative to the total load.
    pub relative_cap_tolerance: f64,
    /// Final absolute charge residual divided by the total load.
    pub normalized_charge_tolerance: f64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_iterations: 64,
            max_evaluations: 512,
            max_line_search: 8,
            max_aux_bytes: 4096,
            max_condition: 1e12,
            relative_cap_tolerance: 1e-11,
            normalized_charge_tolerance: 1e-12,
        }
    }
}

impl Budget {
    fn validate(self) -> Result<(), ComputeError> {
        if self.max_aux_bytes < 512 {
            return Err(ComputeError::TooLarge(
                "RC charge auxiliary byte budget".into(),
            ));
        }
        if self.max_iterations == 0 || self.max_evaluations < 2 || self.max_line_search == 0 {
            return Err(ComputeError::TooLarge(
                "RC charge work budget is empty".into(),
            ));
        }
        if self.max_iterations > 256
            || self.max_evaluations > 4096
            || self.max_line_search > 32
            || !self.max_condition.is_finite()
            || self.max_condition <= 1.0
            || !self.relative_cap_tolerance.is_finite()
            || self.relative_cap_tolerance <= 0.0
            || self.relative_cap_tolerance > 1e-3
            || !self.normalized_charge_tolerance.is_finite()
            || self.normalized_charge_tolerance <= 0.0
            || self.normalized_charge_tolerance > 1e-3
        {
            return Err(ComputeError::Invalid(
                "invalid RC charge solve budget/tolerance".into(),
            ));
        }
        Ok(())
    }
}

/// The driver response and bounded scalar-solver diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct Solution {
    /// NLDM delay, actual pi-root transition and fitted Thevenin waveform.
    pub response: GateResponse,
    /// Completed safeguarded root iterations.
    pub iterations: usize,
    /// NLDM callback evaluations actually consumed.
    pub evaluations: usize,
    /// Final charge difference divided by the total capacitance.
    pub normalized_charge_residual: f64,
    /// Reciprocal normalized local charge derivative at the solution.
    pub condition: f64,
}

struct Charge<'a, F> {
    pi: Pi,
    poles: Poles,
    thresholds: Thresholds,
    resistance: f64,
    eval: &'a mut F,
    evaluations: usize,
    max_evaluations: usize,
}

impl<F: FnMut(f64) -> (f64, f64)> Charge<'_, F> {
    fn residual(&mut self, cap: f64) -> Result<f64, ComputeError> {
        if self.evaluations >= self.max_evaluations {
            return Err(ComputeError::TooLarge(
                "RC charge evaluation budget exceeded".into(),
            ));
        }
        self.evaluations += 1;
        let (delay, slew) = (self.eval)(cap);
        if !delay.is_finite() || !slew.is_finite() || slew <= 0.0 {
            return Err(ComputeError::Invalid(
                "RC charge NLDM value is not finite/positive".into(),
            ));
        }
        let duration =
            slew * self.thresholds.derate / (self.thresholds.upper - self.thresholds.lower);
        let nominal = cap * ramp_voltage(duration, duration, self.resistance * cap);
        let actual = self.pi.near * self.poles.root(duration, duration)
            + self.pi.far * self.poles.far(duration, duration);
        let residual = (nominal - actual) / self.pi.total();
        if !residual.is_finite() {
            return Err(ComputeError::Invalid("RC charge residual overflow".into()));
        }
        Ok(residual)
    }
}

/// Solve a charge-equivalent load and fit its driver waveform using the default bounds.
pub fn gate(
    pi: Pi,
    t: Thresholds,
    resistance: f64,
    eval: impl FnMut(f64) -> (f64, f64),
) -> Result<GateResponse, ComputeError> {
    Ok(solve(pi, t, resistance, Budget::default(), eval)?.response)
}

/// Solve a charge-equivalent load with explicit constant-memory and numerical budgets.
///
/// The charge residual is normalized by the total capacitance. A Newton step is accepted only
/// inside the passive capacitance bracket, with bounded residual-decreasing line search;
/// bisection supplies progress when that step is unusable. An ill-conditioned local residual,
/// exhausted budget, nonpassive model or incompatible waveform is an error, never a pass.
pub fn solve(
    pi: Pi,
    t: Thresholds,
    resistance: f64,
    budget: Budget,
    mut eval: impl FnMut(f64) -> (f64, f64),
) -> Result<Solution, ComputeError> {
    budget.validate()?;
    pi.validate()?;
    t.validate()?;
    if t.output <= t.lower || t.output >= t.upper {
        return Err(ComputeError::Invalid(
            "RC delay threshold outside transition interval".into(),
        ));
    }
    let total = pi.total();
    let (delay, slew) = eval(total);
    if !delay.is_finite() || !slew.is_finite() {
        return Err(ComputeError::Invalid("RC NLDM nonfinite".into()));
    }
    if pi.resistance == 0.0 || pi.far == 0.0 || slew <= 0.0 {
        let duration = slew.max(0.0) * t.derate / (t.upper - t.lower);
        return Ok(Solution {
            response: GateResponse {
                delay,
                slew: slew.max(0.0),
                capacitance: total,
                resistance: 0.0,
                duration,
                root_crossing: t.output * duration,
            },
            iterations: 0,
            evaluations: 1,
            normalized_charge_residual: 0.0,
            condition: 1.0,
        });
    }
    if !resistance.is_finite() || resistance <= 0.0 {
        return Err(ComputeError::Invalid(
            "RC driver has no positive finite resistance".into(),
        ));
    }
    let poles = Poles::new(pi, resistance);
    let mut charge = Charge {
        pi,
        poles,
        thresholds: t,
        resistance,
        eval: &mut eval,
        evaluations: 1,
        max_evaluations: budget.max_evaluations,
    };
    let (mut lo, mut hi) = (pi.near, total);
    let left = charge.residual(lo)?;
    let right = charge.residual(hi)?;
    if left > budget.normalized_charge_tolerance || right < -budget.normalized_charge_tolerance {
        return Err(ComputeError::Invalid(
            "RC charge root outside passive load bracket".into(),
        ));
    }
    let mut cap = total;
    let mut residual = right;
    let mut condition = 1.0;
    let mut converged = None;
    for iteration in 0..budget.max_iterations {
        if residual > 0.0 {
            hi = cap;
        } else {
            lo = cap;
        }
        let step = (total * 1e-5).max(f64::MIN_POSITIVE);
        let a = (cap - step).max(pi.near);
        let b = (cap + step).min(total);
        let slope = if b > a {
            (charge.residual(b)? - charge.residual(a)?) / (b - a)
        } else {
            0.0
        };
        condition = (slope * total).abs().recip();
        if !condition.is_finite() || condition > budget.max_condition {
            return Err(ComputeError::Invalid(
                "RC charge residual is ill-conditioned".into(),
            ));
        }
        if residual.abs() <= budget.normalized_charge_tolerance
            && (residual / slope).abs() <= total * budget.relative_cap_tolerance
        {
            converged = Some(iteration + 1);
            break;
        }
        let mut next = cap - residual / slope;
        let mut accepted = None;
        if next > lo && next < hi {
            for _ in 0..budget.max_line_search {
                let candidate = charge.residual(next)?;
                if candidate.abs() < residual.abs() {
                    accepted = Some((next, candidate));
                    break;
                }
                next = 0.5 * (next + cap);
            }
        }
        let (next, value) = match accepted {
            Some(pair) => pair,
            None => {
                let next = lo + (hi - lo) * 0.5;
                (next, charge.residual(next)?)
            }
        };
        cap = next;
        residual = value;
    }
    let iterations = converged
        .ok_or_else(|| ComputeError::TooLarge("RC charge iteration budget exceeded".into()))?;
    let evaluations = charge.evaluations;
    if evaluations >= budget.max_evaluations {
        return Err(ComputeError::TooLarge(
            "RC charge evaluation budget exceeded".into(),
        ));
    }
    let (delay, slew) = eval(cap);
    let duration = fit_full_duration(slew, cap, resistance, t)?;
    let root_crossing = poles.crossing(duration, t.output);
    let slew = (poles.crossing(duration, t.upper) - poles.crossing(duration, t.lower)) / t.derate;
    if [delay, slew, duration, root_crossing]
        .iter()
        .any(|x| !x.is_finite())
    {
        return Err(ComputeError::Invalid("RC fitted waveform overflow".into()));
    }
    Ok(Solution {
        response: GateResponse {
            delay,
            slew,
            capacitance: cap,
            resistance,
            duration,
            root_crossing,
        },
        iterations,
        evaluations: evaluations + 1,
        normalized_charge_residual: residual,
        condition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Six sequential scalar models; constant storage <1 KiB/case, <=512 NLDM evaluations.
    // Their independent oracle is external transient + original-library STA differential.
    fn model() -> (Pi, Thresholds) {
        (
            Pi {
                near: 0.016,
                far: 0.145,
                resistance: 0.32,
            },
            Thresholds {
                lower: 0.2,
                upper: 0.8,
                input: 0.5,
                output: 0.5,
                derate: 1.0,
            },
        )
    }

    #[test]
    fn budget_exhaustion_is_an_error() {
        let (pi, t) = model();
        let law = |c: f64| (0.08 + 0.6 * c, 0.014 + 0.85 * c);
        for budget in [
            Budget {
                max_aux_bytes: 511,
                ..Budget::default()
            },
            Budget {
                max_iterations: 1,
                ..Budget::default()
            },
            Budget {
                max_evaluations: 3,
                ..Budget::default()
            },
        ] {
            assert!(matches!(
                solve(pi, t, 0.4, budget, law),
                Err(ComputeError::TooLarge(_))
            ));
        }
        assert!(
            solve(
                pi,
                t,
                0.4,
                Budget {
                    max_condition: 1.01,
                    ..Budget::default()
                },
                law
            )
            .is_err()
        );
    }

    #[test]
    fn delay_origin_and_electrical_units_do_not_change_the_solution() {
        let (pi, t) = model();
        let law = |c: f64| (0.08 + 0.6 * c, 0.014 + 0.85 * c);
        let a = solve(pi, t, 0.4, Budget::default(), law).unwrap();
        let b = solve(pi, t, 0.4, Budget::default(), |c| {
            let (d, s) = law(c);
            (d + 1000.0, s)
        })
        .unwrap();
        assert_eq!(a.response.capacitance, b.response.capacitance);
        assert_eq!(a.response.duration, b.response.duration);
        assert!((b.response.delay - a.response.delay - 1000.0).abs() < 1e-12);
        let scaled = solve(
            Pi {
                near: pi.near * 1000.0,
                far: pi.far * 1000.0,
                resistance: pi.resistance / 1000.0,
            },
            t,
            0.4 / 1000.0,
            Budget::default(),
            |c| law(c / 1000.0),
        )
        .unwrap();
        assert!((a.response.capacitance - scaled.response.capacitance / 1000.0).abs() < 1e-12);
        assert!((a.response.slew - scaled.response.slew).abs() < 1e-12);
        assert!(a.evaluations <= Budget::default().max_evaluations);
        assert!(
            a.normalized_charge_residual.abs() <= Budget::default().normalized_charge_tolerance
        );
    }

    #[test]
    fn degenerate_and_invalid_models_are_explicit() {
        let (pi, t) = model();
        let law = |c: f64| (0.08 + 0.6 * c, 0.014 + 0.85 * c);
        let r = gate(
            Pi {
                resistance: 0.0,
                ..pi
            },
            t,
            0.4,
            law,
        )
        .unwrap();
        assert_eq!(r.capacitance, pi.total());
        assert_eq!((r.delay, r.slew), law(pi.total()));
        assert!(gate(pi, t, 0.0, law).is_err());
        assert!(gate(pi, t, 0.4, |_| (f64::NAN, 0.1)).is_err());
        assert!(gate(Pi { near: -1.0, ..pi }, t, 0.4, law).is_err());
    }
}
