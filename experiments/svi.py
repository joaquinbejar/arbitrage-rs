"""Raw SVI slices: the quasi-explicit fit and the static diagnostics.

A slice is the total implied variance of one expiry as a function of the
log forward moneyness k = ln(K / F):

    w(k) = a + b * (rho * (k - m) + sqrt((k - m)^2 + sigma^2))

(Gatheral 2004; Gatheral and Jacquier 2014, https://arxiv.org/abs/1204.0646).

The fit is the quasi-explicit method of Zeliade (De Marco and Martini,
2009): with y = (k - m) / sigma the slice is linear in
(a, d, c) = (a, rho * b * sigma, b * sigma),

    w = a + d * y + c * sqrt(y^2 + 1),

so for each (m, sigma) of a deterministic grid the inner problem is a
weighted least squares over a polytope, solved exactly by enumerating the
faces of the domain (`solve_box_ls`). The outer grid is refined around its
best point a fixed number of times. Nothing here is random.

The diagnostics (`butterfly_g`, `calendar_gaps`) check the declared
conditions on a finite grid of k. They describe that domain; they are not a
proof that the surface is free of static arbitrage everywhere.
"""

from __future__ import annotations

import itertools
import math
from dataclasses import dataclass

import numpy as np

# Lee's moment bound on the wings of the total variance: the slope of w(k)
# as |k| grows is at most 2, so b * (1 + |rho|) <= 2.
LEE_SLOPE_BOUND = 2.0


@dataclass(frozen=True)
class SviSlice:
    """Raw SVI parameters of one expiry; `tau` in years."""

    tau: float
    a: float
    b: float
    rho: float
    m: float
    sigma: float

    def params(self) -> dict:
        return {"tau": self.tau, "a": self.a, "b": self.b, "rho": self.rho, "m": self.m, "sigma": self.sigma}

    def w(self, k: np.ndarray) -> np.ndarray:
        x = k - self.m
        return self.a + self.b * (self.rho * x + np.sqrt(x * x + self.sigma * self.sigma))

    def w_k(self, k: np.ndarray) -> np.ndarray:
        x = k - self.m
        return self.b * (self.rho + x / np.sqrt(x * x + self.sigma * self.sigma))

    def w_kk(self, k: np.ndarray) -> np.ndarray:
        x = k - self.m
        s2 = self.sigma * self.sigma
        return self.b * s2 / (x * x + s2) ** 1.5

    def minimum(self) -> float:
        """The smallest total variance of the slice, a + b sigma sqrt(1 - rho^2)."""
        return self.a + self.b * self.sigma * math.sqrt(max(1.0 - self.rho * self.rho, 0.0))

    def lee_slope(self) -> float:
        return self.b * (1.0 + abs(self.rho))


def butterfly_g(sl: SviSlice, k: np.ndarray) -> np.ndarray:
    """Gatheral-Jacquier g(k); a slice has no butterfly arbitrage on the grid
    where g >= 0 (with w > 0)."""
    w = sl.w(k)
    w1 = sl.w_k(k)
    w2 = sl.w_kk(k)
    return (1.0 - k * w1 / (2.0 * w)) ** 2 - (w1 * w1 / 4.0) * (1.0 / w + 0.25) + w2 / 2.0


def calendar_gaps(slices: list[SviSlice], k: np.ndarray) -> np.ndarray:
    """w(k, tau_{i+1}) - w(k, tau_i) for consecutive slices, one row per pair;
    no calendar arbitrage on the grid where every entry is >= 0."""
    return np.array([slices[i + 1].w(k) - slices[i].w(k) for i in range(len(slices) - 1)])


# ---------------------------------------------------------------- fit


def _constraints(sigma: float, a_max: float) -> tuple[np.ndarray, np.ndarray]:
    """G x <= h for x = (a, d, c): 0 <= c, |d| <= c, |d| <= 2 sigma - c
    (Lee's bound b (1 + |rho|) <= 2 written in (d, c)), 0 <= a <= a_max."""
    g = np.array(
        [
            [0.0, 0.0, -1.0],
            [0.0, 1.0, -1.0],
            [0.0, -1.0, -1.0],
            [0.0, 1.0, 1.0],
            [0.0, -1.0, 1.0],
            [-1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
        ]
    )
    h = np.array([0.0, 0.0, 0.0, LEE_SLOPE_BOUND * sigma, LEE_SLOPE_BOUND * sigma, 0.0, a_max])
    return g, h


def solve_box_ls(design: np.ndarray, target: np.ndarray, weight: np.ndarray, g: np.ndarray, h: np.ndarray):
    """Exact minimiser of sum weight * (design x - target)^2 subject to
    g x <= h, for a three-dimensional x.

    The problem is a convex quadratic programme, so its minimiser is the
    minimiser of the objective restricted to some face of the polytope (a set
    of active constraints of size 0 to 3). Every face is tried: the equality
    constrained least squares on that face is solved through its KKT system,
    infeasible or singular faces are skipped, and the best feasible point is
    returned with the indices of its active constraints. Returns None when no
    face gives a feasible point.
    """
    sw = np.sqrt(weight)
    a_mat = design * sw[:, None]
    y = target * sw
    q = a_mat.T @ a_mat
    lin = a_mat.T @ y
    n = design.shape[1]
    scale = max(1.0, float(np.max(np.abs(h))))
    best = None
    for size in range(0, n + 1):
        for active in itertools.combinations(range(g.shape[0]), size):
            e = g[list(active)] if active else np.zeros((0, n))
            f = h[list(active)] if active else np.zeros(0)
            kkt = np.zeros((n + size, n + size))
            kkt[:n, :n] = 2.0 * q
            kkt[:n, n:] = e.T
            kkt[n:, :n] = e
            rhs = np.concatenate([2.0 * lin, f])
            if np.linalg.matrix_rank(kkt) < n + size:
                continue
            x = np.linalg.solve(kkt, rhs)[:n]
            if np.any(g @ x - h > 1e-12 * scale):
                continue
            r = a_mat @ x - y
            obj = float(r @ r)
            if best is None or obj < best[0]:
                best = (obj, x, active)
        if size == 0 and best is not None:
            # The unconstrained minimiser is feasible: for a convex objective
            # it is the constrained minimiser too.
            break
    if best is None:
        return None
    return best[1], best[2], best[0]


def _inner(k, w_obs, weight, m, sigma):
    y = (k - m) / sigma
    design = np.stack([np.ones_like(y), y, np.sqrt(y * y + 1.0)], axis=1)
    g, h = _constraints(sigma, float(np.max(w_obs)))
    return solve_box_ls(design, w_obs, weight, g, h)


def fit_slice(tau: float, k: np.ndarray, w_obs: np.ndarray, weight: np.ndarray, cfg: dict) -> tuple[SviSlice, dict]:
    """Quasi-explicit fit of one slice.

    The outer grid spans m in [k_min - R, k_max + R] and sigma in
    [sigma_lo * R, sigma_hi * R] (log spaced), R = k_max - k_min, with
    `grid` points per axis; it is refined `refinements` times around the best
    point by shrinking each axis to two cells on each side. Returns the slice
    and a report (objective, active constraints, grid edges hit).
    """
    span = float(np.max(k) - np.min(k))
    m_lo, m_hi = float(np.min(k)) - span, float(np.max(k)) + span
    s_lo, s_hi = math.log(cfg["sigma_lo"] * span), math.log(cfg["sigma_hi"] * span)
    n = cfg["grid"]
    best = None
    shrinks = 0
    recentres = 0
    while True:
        ms = np.linspace(m_lo, m_hi, n)
        ss = np.linspace(s_lo, s_hi, n)
        for i, m in enumerate(ms):
            for j, ls in enumerate(ss):
                res = _inner(k, w_obs, weight, float(m), math.exp(float(ls)))
                if res is None:
                    continue
                x, active, obj = res
                if best is None or obj < best[0]:
                    best = (obj, float(m), float(ls), x, active, ms, ss)
        _, m_best, ls_best, _, _, bms, bss = best
        i = int(np.argmin(np.abs(bms - m_best)))
        j = int(np.argmin(np.abs(bss - ls_best)))
        on_edge = i in (0, n - 1) or j in (0, n - 1)
        dm, ds = ms[1] - ms[0], ss[1] - ss[0]
        if on_edge and bms is ms and recentres < cfg["max_recentres"]:
            # The best point is on the edge of the current window: move the
            # window there without shrinking it.
            recentres += 1
            hm, hs = 0.5 * (m_hi - m_lo), 0.5 * (s_hi - s_lo)
            m_lo, m_hi = m_best - hm, m_best + hm
            s_lo, s_hi = ls_best - hs, ls_best + hs
            continue
        if shrinks == cfg["refinements"]:
            break
        shrinks += 1
        m_lo, m_hi = m_best - 2.0 * dm, m_best + 2.0 * dm
        s_lo, s_hi = ls_best - 2.0 * ds, ls_best + 2.0 * ds
    obj, m, ls, x, active, bms, bss = best
    i = int(np.argmin(np.abs(bms - m)))
    j = int(np.argmin(np.abs(bss - ls)))
    sigma = math.exp(ls)
    a, d, c = (float(v) for v in x)
    b = c / sigma
    rho = d / c if c > 0.0 else 0.0
    sl = SviSlice(tau=tau, a=a, b=b, rho=rho, m=m, sigma=sigma)
    report = {
        "objective": obj,
        "active_constraints": list(active),
        "final_grid_edge": bool(i in (0, n - 1) or j in (0, n - 1)),
    }
    return sl, report
