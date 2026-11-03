"""The reference surface: raw SVI slices fitted to semi-analytic Heston prices.

Everything is predeclared in the configuration passed in (Heston
parameters, expiries, strike grid, weights, price filter, quadrature and
its tolerance); no simulated path or test price enters the fit. The surface
is static: calibrated once at t = 0 with v0, never recalibrated.

Per expiry tau (one slice per day from 1 to `days` days):

1. strikes at k = ln(K / F) on `k_std` * sqrt(v0 tau) (a grid in units of
   the ATM standard deviation);
2. Heston call prices (`heston.price_converged`); a quadrature change above
   `quad_tol` when nodes and cutoff are doubled is a failure;
3. the out-of-the-money price (put below F, call above, by parity at
   r = carry = 0) is inverted to an implied volatility by bisection;
   quotes whose OTM price is below `min_price` are dropped and reported;
4. raw SVI fit on total variance, weights 1 / sigma_iv^2: to first order a
   least squares that is uniform in implied volatility.
"""

from __future__ import annotations

import math

import numpy as np
import torch

import heston
import svi


def _black_otm(fwd, strike, vol):
    """Undiscounted out-of-the-money Black price (call for K >= F)."""
    d1 = (np.log(fwd / strike) + 0.5 * vol * vol) / vol
    d2 = d1 - vol
    n = lambda x: torch.special.ndtr(torch.from_numpy(np.asarray(x, dtype=np.float64))).numpy()  # noqa: E731
    call = fwd * n(d1) - strike * n(d2)
    put = strike * n(-d2) - fwd * n(-d1)
    return np.where(strike >= fwd, call, put)


def implied_vol(fwd, strikes, tau, otm_prices, lo=1e-4, hi=5.0, iterations=200):
    """Bisection on sigma for OTM prices; returns sigma and the price residual."""
    a = np.full_like(strikes, lo)
    b = np.full_like(strikes, hi)
    sq = math.sqrt(tau)
    for _ in range(iterations):
        mid = 0.5 * (a + b)
        too_high = _black_otm(fwd, strikes, mid * sq) > otm_prices
        b = np.where(too_high, mid, b)
        a = np.where(too_high, a, mid)
    sig = 0.5 * (a + b)
    return sig, _black_otm(fwd, strikes, sig * sq) - otm_prices


def calibrate(cfg: dict) -> tuple[list[svi.SviSlice], dict]:
    """Fit one slice per day; returns the slices and the calibration report."""
    p = cfg["heston"]
    spot = cfg["s_ref"]
    report = {"slices": [], "dropped_quotes": 0, "max_quad_change": 0.0, "failures": []}
    slices = []
    for day in range(1, cfg["days"] + 1):
        tau = day / cfg["year_days"]
        fwd = spot  # rate = carry = 0
        std = math.sqrt(p["v0"] * tau)
        k = np.array(cfg["k_std"]) * std
        strikes = fwd * np.exp(k)
        calls, change = heston.price_converged(spot, strikes, tau, p, cfg["quad"])
        report["max_quad_change"] = max(report["max_quad_change"], change)
        if change > cfg["quad_tol"]:
            report["failures"].append(f"day {day}: quadrature change {change:.3e} above {cfg['quad_tol']:.1e}")
        otm = np.where(strikes >= fwd, calls, calls - (fwd - strikes))
        keep = otm >= cfg["min_price"]
        report["dropped_quotes"] += int(np.sum(~keep))
        sig, resid = implied_vol(fwd, strikes[keep], tau, otm[keep])
        if np.max(np.abs(resid)) > cfg["iv_price_tol"]:
            report["failures"].append(f"day {day}: implied volatility inversion residual {np.max(np.abs(resid)):.3e}")
        kk = k[keep]
        w_obs = sig * sig * tau
        sl, fit = svi.fit_slice(tau, kk, w_obs, 1.0 / (sig * sig), cfg["svi_fit"])
        iv_fit = np.sqrt(sl.w(kk) / tau)
        res = iv_fit - sig
        report["slices"].append(
            {
                "day": day,
                "params": sl.params(),
                "quotes": int(np.sum(keep)),
                "iv_rmse": float(np.sqrt(np.mean(res * res))),
                "iv_max_abs_residual": float(np.max(np.abs(res))),
                "residuals_by_k_std": {f"{s:g}": float(r) for s, r in zip(np.array(cfg["k_std"])[keep], res)},
                "fit": fit,
                "atm_iv": float(math.sqrt(sl.w(np.array([0.0]))[0] / tau)),
            }
        )
        slices.append(sl)
    return slices, report


def diagnose(slices: list[svi.SviSlice], k_by_day: dict, cfg: dict) -> dict:
    """Static diagnostics on a finite grid: w > 0, Lee's wing bound,
    butterfly g(k) >= 0 and calendar monotonicity. `k_by_day` gives, per
    slice day, the k range to cover besides the calibration range (the
    realised k of the paths evaluated). These describe that grid only."""
    out = {"failures": [], "min_g": math.inf, "min_w": math.inf, "min_calendar_gap": math.inf, "max_lee_slope": 0.0}
    grids = []
    for sl, day in zip(slices, range(1, len(slices) + 1)):
        std = math.sqrt(cfg["heston"]["v0"] * sl.tau)
        lo, hi = cfg["k_std"][0] * std, cfg["k_std"][-1] * std
        if day in k_by_day:
            lo, hi = min(lo, k_by_day[day][0]), max(hi, k_by_day[day][1])
        k = np.linspace(lo, hi, cfg["diagnostic_points"])
        grids.append(k)
        w = sl.w(k)
        g = svi.butterfly_g(sl, k)
        out["min_w"] = min(out["min_w"], float(np.min(w)), sl.minimum())
        out["min_g"] = min(out["min_g"], float(np.min(g)))
        out["max_lee_slope"] = max(out["max_lee_slope"], sl.lee_slope())
        if np.min(w) <= 0.0 or sl.minimum() <= 0.0:
            out["failures"].append(f"day {day}: non-positive total variance")
        if np.min(g) < 0.0:
            out["failures"].append(f"day {day}: butterfly g(k) < 0 (min {np.min(g):.3e})")
        if sl.lee_slope() > svi.LEE_SLOPE_BOUND:
            out["failures"].append(f"day {day}: Lee slope {sl.lee_slope():.3e} above {svi.LEE_SLOPE_BOUND}")
    # Calendar: on the union of the grids of each consecutive pair.
    for i in range(len(slices) - 1):
        k = np.union1d(grids[i], grids[i + 1])
        gap = float(np.min(slices[i + 1].w(k) - slices[i].w(k)))
        out["min_calendar_gap"] = min(out["min_calendar_gap"], gap)
        if gap < 0.0:
            out["failures"].append(f"days {i + 1}-{i + 2}: calendar w decreases (min gap {gap:.3e})")
    return out
