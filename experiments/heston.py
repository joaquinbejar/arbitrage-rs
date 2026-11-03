"""Semi-analytic Heston prices for European options, and a Monte Carlo check.

Prices use the single-integral representation of Lewis (2000): with
X = ln(S_T / F) and its characteristic function phi,

    C = e^{-r tau} * (F - sqrt(F K) / pi * I),
    I = int_0^inf Re[e^{-i u k} phi(u - i/2)] / (u^2 + 1/4) du,  k = ln(K / F).

phi is written in the form of Albrecher, Mayer, Schoutens and Tistaert
(2007), "The little Heston trap", which keeps the complex logarithm on its
principal branch along the integration path (no branch-cut jump), with
d = sqrt((rho xi i u - kappa)^2 + xi^2 (i u + u^2)) on the principal square
root and g = (kappa - rho xi i u - d) / (kappa - rho xi i u + d).

The integral is truncated at U = cutoff / sqrt(v0 tau) and computed with
composite Gauss-Legendre (numpy's `leggauss`, `nodes` per panel) on the
panels [0, 1/2], [1/2, 1], [1, 2], ... doubling up to U: the factor
1 / (u^2 + 1/4) varies on a scale of 1/2 near zero while the characteristic
function decays on a scale of 1 / sqrt(v0 tau), which is far larger at
short expiries. `price_converged` doubles the nodes per panel and the cutoff
and reports the largest change; the caller decides whether it is within its
declared tolerance.
"""

from __future__ import annotations

import math

import numpy as np


def char_fn(u: np.ndarray, tau: float, p: dict) -> np.ndarray:
    """E[exp(i u X)], X = ln(S_T / F), under Heston (little trap form)."""
    kappa, theta, xi, rho, v0 = p["kappa"], p["theta"], p["xi"], p["rho"], p["v0"]
    iu = 1j * u
    beta = kappa - rho * xi * iu
    d = np.sqrt(beta * beta + xi * xi * (iu + u * u))
    g = (beta - d) / (beta + d)
    e = np.exp(-d * tau)
    c = (kappa * theta / (xi * xi)) * ((beta - d) * tau - 2.0 * np.log((1.0 - g * e) / (1.0 - g)))
    dd = ((beta - d) / (xi * xi)) * (1.0 - e) / (1.0 - g * e)
    return np.exp(c + dd * v0)


def quadrature(cutoff: float, nodes: int) -> tuple[np.ndarray, np.ndarray]:
    """Composite Gauss-Legendre nodes and weights on [0, cutoff]."""
    edges = [0.0, 0.5]
    while edges[-1] < cutoff:
        edges.append(min(2.0 * edges[-1], cutoff))
    x, w = np.polynomial.legendre.leggauss(nodes)
    us, ws = [], []
    for lo, hi in zip(edges[:-1], edges[1:]):
        us.append(lo + 0.5 * (hi - lo) * (x + 1.0))
        ws.append(0.5 * (hi - lo) * w)
    return np.concatenate(us), np.concatenate(ws)


def call_price(spot: float, strikes: np.ndarray, tau: float, p: dict, quad: dict, rate=0.0, carry=0.0) -> np.ndarray:
    """Heston European call prices for several strikes of one expiry."""
    u, wq = quadrature(quad["cutoff"] / math.sqrt(p["v0"] * tau), quad["nodes"])
    fwd = spot * math.exp((rate - carry) * tau)
    k = np.log(np.asarray(strikes, dtype=float) / fwd)
    phi = char_fn(u - 0.5j, tau, p)
    integrand = np.real(np.exp(-1j * np.outer(k, u)) * phi[None, :]) / (u * u + 0.25)[None, :]
    integral = integrand @ wq
    return math.exp(-rate * tau) * (fwd - np.sqrt(fwd * np.asarray(strikes, dtype=float)) / math.pi * integral)


def price_converged(spot, strikes, tau, p, quad, rate=0.0, carry=0.0):
    """Prices at the declared quadrature and the largest absolute change when
    both the nodes and the cutoff are doubled."""
    base = call_price(spot, strikes, tau, p, quad, rate, carry)
    finer = call_price(spot, strikes, tau, p, {"nodes": 2 * quad["nodes"], "cutoff": 2 * quad["cutoff"]}, rate, carry)
    return base, float(np.max(np.abs(finer - base)))


def mc_call(spot, strikes, tau, p, n_paths, substeps_per_day, days_per_year, rng):
    """Full truncation Euler Monte Carlo (the scheme of the M5-06 paths),
    r = carry = 0. Returns the mean discounted payoff per strike and its
    standard error."""
    steps = max(1, round(tau * days_per_year * substeps_per_day))
    dt = tau / steps
    log_s = np.zeros(n_paths)
    v = np.full(n_paths, p["v0"])
    for _ in range(steps):
        z1 = rng.standard_normal(n_paths)
        z2 = p["rho"] * z1 + math.sqrt(1.0 - p["rho"] ** 2) * rng.standard_normal(n_paths)
        vp = np.maximum(v, 0.0)
        log_s += -0.5 * vp * dt + np.sqrt(vp * dt) * z1
        v = v + p["kappa"] * (p["theta"] - vp) * dt + p["xi"] * np.sqrt(vp * dt) * z2
    s_t = spot * np.exp(log_s)
    pay = np.maximum(s_t[None, :] - np.asarray(strikes, dtype=float)[:, None], 0.0)
    return pay.mean(axis=1), pay.std(axis=1, ddof=1) / math.sqrt(n_paths)


def integrated_variance(tau: float, p: dict) -> float:
    """int_0^tau E[v_t] dt, the total variance of the xi = 0 limit."""
    kappa, theta, v0 = p["kappa"], p["theta"], p["v0"]
    return theta * tau + (v0 - theta) * (1.0 - math.exp(-kappa * tau)) / kappa
