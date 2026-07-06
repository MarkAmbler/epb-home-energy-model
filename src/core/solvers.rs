use anyhow::anyhow;
use eqsolver::single_variable::FDNewton;
use interp::{interp_slice, InterpMode};
use roots::{find_root_secant, SimpleConvergency};
use std::sync::Arc;

pub fn fsolve(func: impl Fn(f64) -> f64 + Copy, x0: f64) -> anyhow::Result<f64> {
    let solver = FDNewton::new(func);

    solver.solve(x0).map_err(|e| anyhow::anyhow!(e))
}

pub mod bisect {
    use std::fmt;

    pub fn bisect(
        func: impl Fn(f64) -> anyhow::Result<f64>,
        a: f64,
        b: f64,
        xtol: f64,
    ) -> Result<(f64, RootResults), BisectError> {
        let mut a = a;
        let mut b = b;

        let rtol = 8.881784197001252e-16; // 4 * f64::EPSILON
        let maxiter = 100;

        let mut func_calls = 0;
        let mut iterations = 0;

        let mut fa = func(a)?;
        let mut fb = func(b)?;
        func_calls += 2;

        // 2. Initial bracket validation
        if fa * fb > 0.0 {
            return Err(BisectError::SignError(
                "f(a) and f(b) must have different signs".to_string(),
            ));
        }

        // Quick check if endpoints are already perfectly zero
        if fa == 0.0 {
            return Ok((
                a,
                RootResults {
                    root: a,
                    iterations,
                    function_calls: func_calls,
                    converged: true,
                    flag: "converged".into(),
                },
            ));
        }
        if fb == 0.0 {
            return Ok((
                b,
                RootResults {
                    root: b,
                    iterations,
                    function_calls: func_calls,
                    converged: true,
                    flag: "converged".into(),
                },
            ));
        }

        // Orient interval boundaries such that f(a) < 0
        if fa > 0.0 {
            std::mem::swap(&mut a, &mut b);
            std::mem::swap(&mut fa, &mut fb);
        }

        let mut mid = a;

        // 3. Main Bisection Loop
        while iterations < maxiter {
            iterations += 1;

            // Midpoint calculation designed to minimize floating-point roundoff
            mid = a + (b - a) * 0.5;
            let fmid = func(mid)?;
            func_calls += 1;

            if fmid == 0.0 {
                break;
            } else if fmid < 0.0 {
                a = mid;
            } else {
                b = mid;
            }

            // SciPy's convergence criterion formula
            let delta = (b - a).abs();
            let threshold = xtol + rtol * mid.abs();
            if delta <= threshold {
                break;
            }
        }

        let converged = iterations <= maxiter;
        let flag = if converged {
            "converged".to_string()
        } else {
            format!("Failed to converge after {} iterations.", maxiter)
        };

        let results = RootResults {
            root: mid,
            iterations,
            function_calls: func_calls,
            converged,
            flag,
        };

        if !converged {
            Err(BisectError::ConvergenceError(results))
        } else {
            Ok((mid, results))
        }
    }

    #[derive(Debug, Clone)]
    pub struct RootResults {
        pub root: f64,
        pub iterations: usize,
        pub function_calls: usize,
        pub converged: bool,
        pub flag: String,
    }

    #[derive(Debug)]
    pub enum BisectError {
        SignError(String),
        ConvergenceError(RootResults),
        FuncError(anyhow::Error),
    }

    impl From<anyhow::Error> for BisectError {
        fn from(err: anyhow::Error) -> Self {
            BisectError::FuncError(err)
        }
    }

    impl fmt::Display for BisectError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                BisectError::SignError(msg) => write!(f, "ValueError: {}", msg),
                BisectError::ConvergenceError(res) => write!(f, "RuntimeError: {}", res.flag),
                BisectError::FuncError(err) => write!(f, "Error in passed-in function: {}", err),
            }
        }
    }

    impl std::error::Error for BisectError {}
}

// A viable equivalent of scipy.optimize.root
pub(crate) fn root<const ARGCOUNT: usize>(
    fun: impl Fn(f64, [f64; ARGCOUNT]) -> f64,
    x0: f64,
    args: [f64; ARGCOUNT],
    tol: Option<f64>,
) -> anyhow::Result<f64> {
    let tol = tol.unwrap_or(1e-9);

    let mut convergency = SimpleConvergency {
        eps: tol,
        max_iter: 100,
    };

    let x1 = x0 + 5.0;

    let mut wrapper = |x: f64| -> f64 { fun(x, args) };

    let found_root = find_root_secant(x0, x1, &mut wrapper, &mut convergency)
        .map_err(|e| anyhow::anyhow!("Root optimisation failed: {:?}", e))?;

    Ok(found_root)
}

// Explicit Runge-Kutta 5(4) (Dormand-Prince) coefficients, matching
// scipy.integrate RK45 exactly (scipy/integrate/_ivp/rk.py). The port
// reproduces scipy's step-size control AND its quartic dense-output
// interpolant, so that terminal-event times match scipy to ~machine
// precision. The previous implementation delegated to the
// `differential-equations` crate, whose `dopri5` localizes events on a
// cubic Hermite interpolant and so reported event times ~1e-4 away from
// scipy; that flipped emitter warm-up/hold energy splits and broke 1:1
// parity with the Python reference. See tests below for the comparison.
const RK_N_STAGES: usize = 6;
const RK_ERROR_ESTIMATOR_ORDER: i32 = 4;
const RK_SAFETY: f64 = 0.9;
const RK_MIN_FACTOR: f64 = 0.2;
const RK_MAX_FACTOR: f64 = 10.0;

const RK_C: [f64; 6] = [0.0, 1.0 / 5.0, 3.0 / 10.0, 4.0 / 5.0, 8.0 / 9.0, 1.0];
const RK_A: [[f64; 5]; 6] = [
    [0.0, 0.0, 0.0, 0.0, 0.0],
    [1.0 / 5.0, 0.0, 0.0, 0.0, 0.0],
    [3.0 / 40.0, 9.0 / 40.0, 0.0, 0.0, 0.0],
    [44.0 / 45.0, -56.0 / 15.0, 32.0 / 9.0, 0.0, 0.0],
    [
        19372.0 / 6561.0,
        -25360.0 / 2187.0,
        64448.0 / 6561.0,
        -212.0 / 729.0,
        0.0,
    ],
    [
        9017.0 / 3168.0,
        -355.0 / 33.0,
        46732.0 / 5247.0,
        49.0 / 176.0,
        -5103.0 / 18656.0,
    ],
];
const RK_B: [f64; 6] = [
    35.0 / 384.0,
    0.0,
    500.0 / 1113.0,
    125.0 / 192.0,
    -2187.0 / 6784.0,
    11.0 / 84.0,
];
const RK_E: [f64; 7] = [
    -71.0 / 57600.0,
    0.0,
    71.0 / 16695.0,
    -71.0 / 1920.0,
    17253.0 / 339200.0,
    -22.0 / 525.0,
    1.0 / 40.0,
];
const RK_P: [[f64; 4]; 7] = [
    [
        1.0,
        -8048581381.0 / 2820520608.0,
        8663915743.0 / 2820520608.0,
        -12715105075.0 / 11282082432.0,
    ],
    [0.0, 0.0, 0.0, 0.0],
    [
        0.0,
        131558114200.0 / 32700410799.0,
        -68118460800.0 / 10900136933.0,
        87487479700.0 / 32700410799.0,
    ],
    [
        0.0,
        -1754552775.0 / 470086768.0,
        14199869525.0 / 1410260304.0,
        -10690763975.0 / 1880347072.0,
    ],
    [
        0.0,
        127303824393.0 / 49829197408.0,
        -318862633887.0 / 49829197408.0,
        701980252875.0 / 199316789632.0,
    ],
    [
        0.0,
        -282668133.0 / 205662961.0,
        2019193451.0 / 616988883.0,
        -1453857185.0 / 822651844.0,
    ],
    [
        0.0,
        40617522.0 / 29380423.0,
        -110615467.0 / 29380423.0,
        69997945.0 / 29380423.0,
    ],
];

/// RMS norm, matching scipy's `_ivp.common.norm`.
fn rms_norm(x: &[f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
}

/// Empirically select a good initial step, matching scipy's
/// `_ivp.common.select_initial_step`.
#[allow(clippy::too_many_arguments)]
fn select_initial_step(
    func: &(dyn Fn(f64, &[f64]) -> Vec<f64> + Send + Sync),
    t0: f64,
    y0: &[f64],
    t_bound: f64,
    f0: &[f64],
    direction: f64,
    order: i32,
    rtol: f64,
    atol: f64,
) -> f64 {
    if y0.is_empty() {
        return f64::INFINITY;
    }
    let interval_length = (t_bound - t0).abs();
    if interval_length == 0.0 {
        return 0.0;
    }

    let scale: Vec<f64> = y0.iter().map(|y| atol + y.abs() * rtol).collect();
    let d0 = rms_norm(&y0.iter().zip(&scale).map(|(y, s)| y / s).collect::<Vec<_>>());
    let d1 = rms_norm(&f0.iter().zip(&scale).map(|(f, s)| f / s).collect::<Vec<_>>());

    let h0 = if d0 < 1e-5 || d1 < 1e-5 {
        1e-6
    } else {
        0.01 * d0 / d1
    };
    let h0 = h0.min(interval_length);

    let y1: Vec<f64> = y0
        .iter()
        .zip(f0)
        .map(|(y, f)| y + h0 * direction * f)
        .collect();
    let f1 = func(t0 + h0 * direction, &y1);
    let d2 = rms_norm(
        &f1.iter()
            .zip(f0)
            .zip(&scale)
            .map(|((f1, f0), s)| (f1 - f0) / s)
            .collect::<Vec<_>>(),
    ) / h0;

    let h1 = if d1 <= 1e-15 && d2 <= 1e-15 {
        (1e-6f64).max(h0 * 1e-3)
    } else {
        (0.01 / d1.max(d2)).powf(1.0 / (order as f64 + 1.0))
    };

    (100.0 * h0).min(h1).min(interval_length)
}

/// Perform one Dormand-Prince step, returning the higher-order solution, the
/// derivative at the step end, and the full stage matrix `K` (7 rows, the
/// last being the FSAL derivative). Matches scipy's `_ivp.rk.rk_step`.
fn rk_step(
    func: &(dyn Fn(f64, &[f64]) -> Vec<f64> + Send + Sync),
    t: f64,
    y: &[f64],
    f: &[f64],
    h: f64,
) -> (Vec<f64>, Vec<f64>, Vec<Vec<f64>>) {
    let n = y.len();
    let mut k: Vec<Vec<f64>> = vec![vec![0.0; n]; RK_N_STAGES + 1];
    k[0] = f.to_vec();

    for s in 1..RK_N_STAGES {
        let mut y_stage = vec![0.0; n];
        for (i, ys) in y_stage.iter_mut().enumerate() {
            let mut acc = 0.0;
            for j in 0..s {
                acc += RK_A[s][j] * k[j][i];
            }
            *ys = y[i] + h * acc;
        }
        k[s] = func(t + RK_C[s] * h, &y_stage);
    }

    let mut y_new = vec![0.0; n];
    for (i, yn) in y_new.iter_mut().enumerate() {
        let mut acc = 0.0;
        for j in 0..RK_N_STAGES {
            acc += RK_B[j] * k[j][i];
        }
        *yn = y[i] + h * acc;
    }

    let f_new = func(t + h, &y_new);
    k[RK_N_STAGES] = f_new.clone();

    (y_new, f_new, k)
}

/// Evaluate the RK45 quartic dense-output interpolant at time `t` over a step
/// starting at `t_old` with length `h`, given `Q = K.T @ P`. Matches scipy's
/// `RkDenseOutput`.
fn dense_output(t: f64, t_old: f64, h: f64, y_old: &[f64], q: &[[f64; 4]]) -> Vec<f64> {
    let x = (t - t_old) / h;
    // p = [x, x^2, x^3, x^4]
    let mut p = [0.0; 4];
    let mut acc = x;
    for slot in p.iter_mut() {
        *slot = acc;
        acc *= x;
    }
    y_old
        .iter()
        .zip(q)
        .map(|(y0, qi)| y0 + h * (qi[0] * p[0] + qi[1] * p[1] + qi[2] * p[2] + qi[3] * p[3]))
        .collect()
}

/// Root-finder matching `scipy.optimize.brentq` (Zeros/brentq.c). Used to
/// localize events on the dense-output interpolant, exactly as scipy does.
fn brentq(
    mut f: impl FnMut(f64) -> f64,
    xa: f64,
    xb: f64,
    xtol: f64,
    rtol: f64,
    max_iter: usize,
) -> f64 {
    let (mut xpre, mut xcur) = (xa, xb);
    let mut xblk = 0.0;
    let mut fpre = f(xpre);
    let mut fcur = f(xcur);
    let mut fblk = 0.0;
    let mut spre = 0.0;
    let mut scur = 0.0;

    if fpre == 0.0 {
        return xpre;
    }
    if fcur == 0.0 {
        return xcur;
    }

    for _ in 0..max_iter {
        if fpre * fcur < 0.0 {
            xblk = xpre;
            fblk = fpre;
            spre = xcur - xpre;
            scur = xcur - xpre;
        }
        if fblk.abs() < fcur.abs() {
            xpre = xcur;
            xcur = xblk;
            xblk = xpre;
            fpre = fcur;
            fcur = fblk;
            fblk = fpre;
        }

        let delta = (xtol + rtol * xcur.abs()) / 2.0;
        let sbis = (xblk - xcur) / 2.0;
        if fcur == 0.0 || sbis.abs() < delta {
            return xcur;
        }

        if spre.abs() > delta && fcur.abs() < fpre.abs() {
            let stry = if xpre == xblk {
                // interpolate (secant)
                -fcur * (xcur - xpre) / (fcur - fpre)
            } else {
                // extrapolate (inverse quadratic)
                let dpre = (fpre - fcur) / (xpre - xcur);
                let dblk = (fblk - fcur) / (xblk - xcur);
                -fcur * (fblk * dblk - fpre * dpre) / (dblk * dpre * (fblk - fpre))
            };
            if 2.0 * stry.abs() < spre.abs().min(3.0 * sbis.abs() - delta) {
                spre = scur;
                scur = stry;
            } else {
                spre = sbis;
                scur = sbis;
            }
        } else {
            spre = sbis;
            scur = sbis;
        }

        xpre = xcur;
        fpre = fcur;
        if scur.abs() > delta {
            xcur += scur;
        } else {
            xcur += if sbis > 0.0 { delta } else { -delta };
        }

        fcur = f(xcur);
    }

    xcur
}

/// Native port of `scipy.integrate.solve_ivp` restricted to method RK45 with
/// terminal events (the only variant this codebase uses). Reproduces scipy's
/// adaptive stepping, quartic dense output and Brent event localization so
/// outputs match the Python reference to ~machine precision.
pub fn solve_ivp(
    func: Arc<dyn Fn(f64, &[f64]) -> Vec<f64> + Send + Sync>,
    t_span: (f64, f64),
    y0: &[f64],
    events: Option<TerminatingEvents>,
    rtol: Option<f64>,
    atol: Option<f64>,
) -> anyhow::Result<OdeResult> {
    let rtol = rtol.unwrap_or(1e-3);
    let atol = atol.unwrap_or(1e-6);
    let (t0, tf) = t_span;
    let func_ref = func.as_ref();

    // Event functions and their crossing direction (all terminal).
    let event_funcs: Vec<Arc<dyn Fn(f64, &[f64]) -> f64 + Send + Sync>> =
        events.as_ref().map(|e| e.funcs.clone()).unwrap_or_default();
    let event_direction: f64 = match events.and_then(|e| e.direction).unwrap_or_default() {
        TerminateDirection::Both => 0.0,
        TerminateDirection::Positive => 1.0,
        TerminateDirection::Negative => -1.0,
    };

    let direction = if tf > t0 {
        1.0
    } else if tf < t0 {
        -1.0
    } else {
        1.0
    };

    let mut t = t0;
    let mut y = y0.to_vec();
    let mut f = func_ref(t, &y);
    let error_exponent = -1.0 / (RK_ERROR_ESTIMATOR_ORDER as f64 + 1.0);

    let mut h_abs = select_initial_step(
        func_ref,
        t0,
        &y,
        tf,
        &f,
        direction,
        RK_ERROR_ESTIMATOR_ORDER,
        rtol,
        atol,
    );

    // Recorded solution points; scipy seeds with the initial condition.
    let mut ts: Vec<f64> = vec![t0];
    let mut ys: Vec<Vec<f64>> = vec![y0.to_vec()];

    // Event function values at the current time.
    let mut g: Vec<f64> = event_funcs.iter().map(|e| e(t0, &y)).collect();

    let mut t_event: Option<f64> = None;

    loop {
        // ----- one adaptive Dormand-Prince step (scipy RungeKutta._step_impl) -----
        let t_old = t;
        let y_old = y.clone();

        let min_step = 10.0
            * (if direction > 0.0 {
                t.next_up() - t
            } else {
                t.next_down() - t
            })
            .abs();

        if h_abs < min_step {
            h_abs = min_step;
        }

        let mut step_accepted = false;
        let mut step_rejected = false;
        let mut y_new = y.clone();
        let mut f_new = f.clone();
        let mut k: Vec<Vec<f64>> = Vec::new();
        let mut t_new = t;

        while !step_accepted {
            if h_abs < min_step {
                return Err(anyhow!(
                    "solve_ivp: required step size is less than spacing between numbers"
                ));
            }

            let mut h = h_abs * direction;
            t_new = t + h;
            if direction * (t_new - tf) > 0.0 {
                t_new = tf;
            }
            h = t_new - t;
            h_abs = h.abs();

            let (yn, fn_, kk) = rk_step(func_ref, t, &y, &f, h);

            let scale: Vec<f64> = y
                .iter()
                .zip(&yn)
                .map(|(yi, yni)| atol + yi.abs().max(yni.abs()) * rtol)
                .collect();
            // error estimate = h * (K.T @ E), normalized by scale
            let n = y.len();
            let mut err = vec![0.0; n];
            for (i, ei) in err.iter_mut().enumerate() {
                let mut acc = 0.0;
                for (j, kj) in kk.iter().enumerate() {
                    acc += kj[i] * RK_E[j];
                }
                *ei = acc * h / scale[i];
            }
            let error_norm = rms_norm(&err);

            if error_norm < 1.0 {
                let mut factor = if error_norm == 0.0 {
                    RK_MAX_FACTOR
                } else {
                    RK_MAX_FACTOR.min(RK_SAFETY * error_norm.powf(error_exponent))
                };
                if step_rejected {
                    factor = factor.min(1.0);
                }
                h_abs *= factor;
                y_new = yn;
                f_new = fn_;
                k = kk;
                step_accepted = true;
            } else {
                h_abs *= RK_MIN_FACTOR.max(RK_SAFETY * error_norm.powf(error_exponent));
                step_rejected = true;
            }
        }

        t = t_new;
        y = y_new;
        f = f_new;
        let h_step = t - t_old;
        let status_finished = direction * (t - tf) >= 0.0;

        // ----- event detection over the full step [t_old, t] (scipy solve_ivp loop) -----
        let mut terminate = false;
        if !event_funcs.is_empty() {
            let g_new: Vec<f64> = event_funcs.iter().map(|e| e(t, &y)).collect();
            let active: Vec<usize> = (0..event_funcs.len())
                .filter(|&i| {
                    let up = g[i] <= 0.0 && g_new[i] >= 0.0;
                    let down = g[i] >= 0.0 && g_new[i] <= 0.0;
                    (up && event_direction > 0.0)
                        || (down && event_direction < 0.0)
                        || ((up || down) && event_direction == 0.0)
                })
                .collect();

            if !active.is_empty() {
                // Build the quartic dense-output coefficients Q = K.T @ P for this step.
                let n = y.len();
                let q: Vec<[f64; 4]> = (0..n)
                    .map(|i| {
                        let mut row = [0.0; 4];
                        for (c, slot) in row.iter_mut().enumerate() {
                            let mut acc = 0.0;
                            for (j, kj) in k.iter().enumerate() {
                                acc += kj[i] * RK_P[j][c];
                            }
                            *slot = acc;
                        }
                        row
                    })
                    .collect();

                // Localize each active (terminal) event via Brent on the interpolant.
                let eps = f64::EPSILON;
                let mut earliest = f64::INFINITY;
                for &idx in &active {
                    let ev = event_funcs[idx].clone();
                    let root = brentq(
                        |tt| {
                            let yy = dense_output(tt, t_old, h_step, &y_old, &q);
                            ev(tt, &yy)
                        },
                        t_old,
                        t,
                        4.0 * eps,
                        4.0 * eps,
                        100,
                    );
                    if root < earliest {
                        earliest = root;
                    }
                }

                t = earliest;
                y = dense_output(earliest, t_old, h_step, &y_old, &q);
                t_event = Some(earliest);
                terminate = true;
            }

            g = g_new;
        }

        ts.push(t);
        ys.push(y.clone());

        if terminate || status_finished {
            break;
        }
    }

    Ok(OdeResult {
        y: transpose(ys),
        t_event,
        t: ts,
    })
}

fn transpose(matrix: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    if matrix.is_empty() || matrix[0].is_empty() {
        return Vec::new();
    }

    let num_columns = matrix[0].len();

    (0..num_columns)
        .map(|col_idx| matrix.iter().map(|row| row[col_idx]).collect::<Vec<f64>>())
        .collect()
}

pub struct OdeResult {
    pub y: Vec<Vec<f64>>,
    pub t_event: Option<f64>,
    pub t: Vec<f64>,
}

pub struct TerminatingEvents {
    funcs: Vec<Arc<dyn Fn(f64, &[f64]) -> f64 + Send + Sync>>,
    direction: Option<TerminateDirection>,
}

impl TerminatingEvents {
    pub fn new(
        funcs: Vec<Arc<dyn Fn(f64, &[f64]) -> f64 + Send + Sync>>,
        direction: Option<TerminateDirection>,
    ) -> Self {
        Self { funcs, direction }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub enum TerminateDirection {
    #[default]
    Both,
    Positive,
    Negative,
}

pub fn interp1d<'a>(
    x: Vec<f64>,
    y: Vec<f64>,
    fill_value: Interp1dFillValue,
) -> Arc<dyn Fn(&[f64]) -> Vec<f64> + Send + Sync> {
    let interp_mode = match fill_value {
        Interp1dFillValue::Extrapolate => InterpMode::Extrapolate,
        Interp1dFillValue::FillValues(values) => InterpMode::Constant(values.0),
    };

    let func =
        move |x_new: &[f64]| interp_slice(&x, &y, x_new.try_into().unwrap(), &interp_mode).to_vec();

    Arc::new(func)
}

pub enum Interp1dFillValue {
    Extrapolate,
    FillValues((f64, f64)),
}

#[cfg(test)]
mod rk45_scipy_parity_tests {
    use super::*;

    // Reference values produced by scipy.integrate.solve_ivp(method="RK45",
    // rtol=1e-3, atol=1e-6) on the two-radiator emitter ODE
    //   dy/dt = (P - 0.08*y^1.2 - 0.1*y^1.3)/0.14,  y0=0,  t in [0, 0.5]
    // (thermal_mass and c/n from demo_FHS_hp_temp_output_over_upper_limit).
    // scipy is the oracle here because the Python HEM reference calls scipy's
    // solve_ivp directly. These guard against regression of the native RK45
    // port back towards the cubic-Hermite event localization that broke parity.
    fn emitter_rhs(p: f64) -> Arc<dyn Fn(f64, &[f64]) -> Vec<f64> + Send + Sync> {
        let c_n = [(0.08_f64, 1.2_f64), (0.1_f64, 1.3_f64)];
        Arc::new(move |_t, y: &[f64]| {
            let d = 0f64.max(y[0]);
            vec![(p - c_n.iter().map(|&(c, n)| c * d.powf(n)).sum::<f64>()) / 0.14]
        })
    }

    #[test]
    fn trajectory_endpoints_match_scipy() {
        // (P, scipy y(0.5))
        let cases = [
            (1.0, 2.5125725041),
            (5.0, 10.7847608096),
            (8.0, 16.3476129110),
            (20.0, 36.2955583377),
            (50.0, 79.1658290011),
        ];
        for (p, expected) in cases {
            let res = solve_ivp(emitter_rhs(p), (0.0, 0.5), &[0.0], None, None, None).unwrap();
            let y_final = *res.y[0].last().unwrap();
            assert!(
                (y_final - expected).abs() < 1e-9,
                "P={p}: y_final={y_final}, scipy={expected}"
            );
        }
    }

    #[test]
    fn terminal_event_time_matches_scipy() {
        // (P, scipy t_event for event y=20). scipy localizes on its quartic
        // dense output; the port must reproduce these to ~machine precision.
        let cases = [
            (12.0, 0.3579470621341179),
            (20.0, 0.17281934956270112),
            (30.0, 0.10635504525556469),
            (50.0, 0.060318407848382175),
        ];
        for (p, expected) in cases {
            let event: Arc<dyn Fn(f64, &[f64]) -> f64 + Send + Sync> =
                Arc::new(|_t, y: &[f64]| y[0] - 20.0);
            let events = TerminatingEvents::new(vec![event], None);
            let res =
                solve_ivp(emitter_rhs(p), (0.0, 0.5), &[0.0], Some(events), None, None).unwrap();
            let te = res.t_event.expect("event should have fired");
            assert!(
                (te - expected).abs() < 1e-10,
                "P={p}: t_event={te}, scipy={expected} (diff {:.2e})",
                (te - expected).abs()
            );
            // On a terminal event the final state is truncated to the crossing.
            let y_final = *res.y[0].last().unwrap();
            assert!((y_final - 20.0).abs() < 1e-9, "P={p}: y_final={y_final}");
        }
    }

    #[test]
    fn linear_event_is_exact() {
        // dy/dt = 1, y0=0 -> event y=3.5 at exactly t=3.5.
        let f: Arc<dyn Fn(f64, &[f64]) -> Vec<f64> + Send + Sync> = Arc::new(|_t, _y| vec![1.0]);
        let event: Arc<dyn Fn(f64, &[f64]) -> f64 + Send + Sync> =
            Arc::new(|_t, y: &[f64]| y[0] - 3.5);
        let events = TerminatingEvents::new(vec![event], None);
        let res = solve_ivp(f, (0.0, 10.0), &[0.0], Some(events), None, None).unwrap();
        assert!((res.t_event.unwrap() - 3.5).abs() < 1e-12);
        assert!((*res.y[0].last().unwrap() - 3.5).abs() < 1e-12);
    }
}
