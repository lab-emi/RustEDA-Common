// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! Certified replay of the threshold bisections of [`super::Poles::crossing`] and of the sink
//! crossings of [`super::wire_from_driver`] (STA-REQ-0729).
//!
//! Both bisections halve `[lo, hi]` and keep the half whose midpoint `m` has `f(m) < p`, for an
//! evaluated waveform `f`; the result is a function of these decisions alone. The replay runs the
//! same loop and takes a decision without evaluating `f(m)` only when an evaluation made earlier
//! proves it, so it returns the bisection's result bit for bit:
//!
//! * The exact waveform `g` of the evaluated parameters is nondecreasing. The ramp response of a
//!   network is the running mean over the ramp of its step response, whose derivative is the
//!   impulse response `h(t) = w/a e^(-t/a) + (1-w)/b e^(-t/b)` of the pi root; a sink adds the
//!   convolution with the positive kernel `e^(-t/tau)/tau`. With `0 <= w <= 1`, `h >= 0`. With
//!   `w > 1` and `a > b`, the negative fast term decays first, so `h >= 0` when `h(0) >= 0`,
//!   that is `w b >= (w - 1) a`. Other weights are not replayed.
//! * Every evaluation `f(x)` differs from `g(x)` by at most `E`, the bound of [`root_error`] and
//!   [`load_error`], when every parameter is zero or lies in `[1e-30, 1e30]` (no overflow, no NaN).
//!   A ramp evaluates `x/d * (1 + expm1(-x/tau)/(x/tau))`, `1 + k exp(-(x-d)/tau)` with
//!   `|k| <= 1`, or `-expm1(-x/tau)`, each within 7 units of `f64::EPSILON` of its exact value
//!   when `exp` and `expm1` are within 2 ulp (glibc documents 1 ulp): first-order relative errors
//!   of `u/(1-e^-u) * u e^-u <= 1`, `v e^-v <= 1` and the rounding of each operation. A pole
//!   cascade `(p r_p - tau r_tau) / (p - tau)` amplifies them by `(p + tau) / |p - tau|`, and the
//!   weights by `|w|` and `|1 - w|`. `E` budgets 64 units per ramp, more than nine times that.
//! * An evaluated `f(x) < p - 3E` proves `f(m) < p` for every `m <= x`:
//!   `f(m) <= g(m) + E <= g(x) + E <= f(x) + 2E < p`. An evaluated `f(x) >= p + 3E` proves
//!   `f(m) >= p` for every `m >= x` in the same way (`3E` also covers the rounding of `p +- 3E`).
//!
//! Safeguarded Newton steps place such evaluations close to the crossing before the replay, so
//! that it evaluates about ten times per crossing instead of 44. They only decide how many
//! evaluations the replay saves, never a decision. With `RUSTEDA_RC_VERIFY` set to a non-empty
//! value other than `0`, every replay is also run as the plain bisection and must agree bit for
//! bit (a panic otherwise).

use super::Ramp;

/// The error budget per evaluated ramp, see the module documentation.
const RAMP_ERROR: f64 = 64.0 * f64::EPSILON;

/// Whether `x` is zero or lies in `[1e-30, 1e30]`, the range of the error model.
pub(super) fn modelled(x: f64) -> bool {
    x == 0.0 || (1e-30..=1e30).contains(&x)
}

/// Whether the root response of the poles `a >= b > 0` with the slow-pole weight `w` has a
/// nonnegative impulse response, so that its ramp responses, and their cascades with a sink
/// pole, are nondecreasing.
pub(super) fn monotone(a: f64, b: f64, w: f64) -> bool {
    if !(modelled(a) && modelled(b) && a > 0.0 && b > 0.0 && w.is_finite()) {
        return false;
    }
    if (0.0..=1.0).contains(&w) {
        return true;
    }
    // h(0) = w/a + (1-w)/b >= 0, with a margin far above the rounding of both products.
    w > 1.0 && a > b && w * b > (w - 1.0) * a * (1.0 + 1e-9)
}

/// The bound `E` of the evaluated root response `w r_a + (1 - w) r_b`.
pub(super) fn root_error(w: f64) -> f64 {
    RAMP_ERROR * (w.abs() + (1.0 - w).abs())
}

/// The bound `E` of the evaluated sink response `w c_a + (1 - w) c_b`, with the cascades
/// `c_p = (p r_p - tau r_tau) / (p - tau)` of distinct poles.
pub(super) fn load_error(a: f64, b: f64, w: f64, tau: f64) -> f64 {
    let gain = |p: f64| (p + tau) / (p - tau).abs();
    RAMP_ERROR * (w.abs() * gain(a) + (1.0 - w).abs() * gain(b))
}

/// The evaluations that decide bisection steps without evaluating: every midpoint at or below
/// `below` is below `p`, every midpoint at or above `above` is not.
#[derive(Debug, Clone, Copy)]
pub(super) struct Certificate {
    p: f64,
    margin: f64,
    below: f64,
    above: f64,
}

impl Certificate {
    /// No decision yet for the crossing of `p` by an evaluation within `error` of a
    /// nondecreasing waveform.
    pub(super) fn new(p: f64, error: f64) -> Self {
        Self {
            p,
            margin: 3.0 * error,
            below: f64::NEG_INFINITY,
            above: f64::INFINITY,
        }
    }

    /// Records the evaluation `v = f(x)`.
    pub(super) fn record(&mut self, x: f64, v: f64) {
        if v < self.p - self.margin {
            self.below = self.below.max(x);
        } else if v >= self.p + self.margin {
            self.above = self.above.min(x);
        }
    }

    fn decide(&self, x: f64) -> Option<bool> {
        if x <= self.below {
            Some(true)
        } else if x >= self.above {
            Some(false)
        } else {
            None
        }
    }

    /// Records evaluations near the crossing in `[lo, hi]`: safeguarded Newton steps from `x0`
    /// with the value and slope estimate `fs(x)`, then one evaluation on each side of the last
    /// iterate once it lies within the band of `p` that proves nothing. The value of `fs` must be
    /// the replayed evaluation itself; its slope only steers the steps.
    pub(super) fn probe(
        &mut self,
        fs: impl Fn(f64) -> (f64, f64),
        lo: f64,
        hi: f64,
        x0: f64,
        tolerance: f64,
    ) {
        let (mut a, mut b) = (lo, hi);
        let mut x = if x0 > lo && x0 < hi {
            x0
        } else {
            lo + (hi - lo) * 0.5
        };
        for _ in 0..16 {
            if self.above - self.below <= tolerance {
                return;
            }
            let (v, s) = fs(x);
            if !v.is_finite() {
                return;
            }
            self.record(x, v);
            if v < self.p {
                a = a.max(x);
            } else {
                b = b.min(x);
            }
            if (v - self.p).abs() <= self.margin {
                let step = if s > 0.0 && s.is_finite() {
                    (2.0 * self.margin / s).max(tolerance * 0.25)
                } else {
                    tolerance
                };
                for side in [-1.0, 1.0] {
                    let mut d = step;
                    for _ in 0..4 {
                        let y = x + side * d;
                        if !(y > lo && y < hi) {
                            break;
                        }
                        let (vy, _) = fs(y);
                        if !vy.is_finite() {
                            break;
                        }
                        self.record(y, vy);
                        if (side < 0.0 && self.below >= y) || (side > 0.0 && self.above <= y) {
                            break;
                        }
                        d *= 4.0;
                    }
                }
                return;
            }
            let next = x - (v - self.p) / s;
            x = if next > a && next < b {
                next
            } else {
                a + (b - a) * 0.5
            };
        }
    }

    /// The bisection of `[lo, hi]` for the crossing of `p` by `f` (at most `steps` halvings to
    /// `tolerance`), with the decisions this certificate proves taken without evaluating `f`.
    pub(super) fn bisect(
        mut self,
        f: impl Fn(f64) -> f64,
        mut lo: f64,
        mut hi: f64,
        tolerance: f64,
        steps: usize,
    ) -> f64 {
        for _ in 0..steps {
            if hi - lo <= tolerance {
                break;
            }
            let mid = lo + (hi - lo) * 0.5;
            let below = match self.decide(mid) {
                Some(below) => below,
                None => {
                    let v = f(mid);
                    self.record(mid, v);
                    v < self.p
                }
            };
            if below {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo + (hi - lo) * 0.5
    }
}

/// The plain bisection that [`Certificate::bisect`] replays.
pub(super) fn bisect(
    f: impl Fn(f64) -> f64,
    p: f64,
    mut lo: f64,
    mut hi: f64,
    tolerance: f64,
    steps: usize,
) -> f64 {
    for _ in 0..steps {
        if hi - lo <= tolerance {
            break;
        }
        let mid = lo + (hi - lo) * 0.5;
        if f(mid) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo + (hi - lo) * 0.5
}

/// Whether `RUSTEDA_RC_VERIFY` asks to check every replay against the plain bisection.
pub(super) fn verify() -> bool {
    static VERIFY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERIFY.get_or_init(|| {
        std::env::var_os("RUSTEDA_RC_VERIFY").is_some_and(|v| !v.is_empty() && v != "0")
    })
}

/// Panics unless the replayed crossing `replayed` equals the plain bisection's `plain`.
pub(super) fn check(replayed: f64, plain: f64, what: &str) {
    assert!(
        replayed.to_bits() == plain.to_bits(),
        "RC replay of {what} differs from its bisection: {replayed:e} != {plain:e}"
    );
}

impl Ramp {
    /// `(self.at(time), slope)`: the value bit-identical to [`Ramp::at`], with the derivative of
    /// the ramp response, which only steers [`Certificate::probe`].
    #[inline]
    pub(super) fn at_slope(self, time: f64) -> (f64, f64) {
        let (duration, tau) = (self.duration, self.tau);
        if time <= 0.0 {
            return (0.0, 0.0);
        }
        if tau == 0.0 {
            return if duration == 0.0 {
                (1.0, 0.0)
            } else if time < duration {
                ((time / duration).min(1.0), 1.0 / duration)
            } else {
                ((time / duration).min(1.0), 0.0)
            };
        }
        if duration == 0.0 {
            let v = -(-time / tau).exp_m1();
            return (v, (1.0 - v) / tau);
        }
        if time <= duration {
            // `charged_fraction(time, tau)` and the slope share `expm1(-time / tau)`.
            let e = (-time / tau).exp_m1();
            let v = time / duration * super::charged_given(time / tau, e);
            (v, -e / duration)
        } else {
            let e = (-(time - duration) / tau).exp();
            (1.0 + self.late * e, -self.late * e / tau)
        }
    }

    /// The late-time factor `tau / duration * expm1(-duration / tau)` (0 without a pole time or
    /// a duration).
    pub(super) fn late(self) -> f64 {
        self.late
    }
}
