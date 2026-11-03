"""Pricing counterpart for the deep hedging experiments (M5-06b, #36).

A counterpart prices one European option and gives its Greeks from ONE
price function under a declared surface dynamics. The experiment talks to
it only through `Counterpart`, so the reference implementation here can be
replaced by market-maker-rs (`MarketMakerRsCounterpart`, empty until
market-maker-rs#95 and #96 exist).

Conventions:

- Prices and the strike are in the quote currency per unit of underlying,
  contract multiplier 1. Time is in days; tau = (expiry_days - as_of_days)
  / year, year = 365.
- rate and carry are constant, continuously compounded, fixed in the
  counterpart's configuration; F = S * exp((rate - carry) * tau).
- The surface is total implied variance w(k, tau), k = ln(K / F).
- Dynamics, when the spot moves:
  - "sticky-moneyness": w is read at the k of the current forward, so
    dw/dS = -w_k / S;
  - "sticky-strike": w is read at the k of the calibration forward
    F_ref = S_ref * exp((rate - carry) * tau), so dw/dS = 0.
- V(S, t) = BSM(S, K, w(k, tau)); every Greek is a derivative of that V:
  - delta = dV/dS and gamma = d2V/dS2, totals under the dynamics (gamma
    includes the vanna and volga terms and w_kk);
  - vega = dV/ds for a parallel shift s of the implied volatility of the
    whole surface, per 1.00 of volatility;
  - theta = dV/dt in calendar time, per year, with w held as a function
    of (k, tau) and rate and carry constant.
- At tau = 0 (as_of == expiry) the surface is not consulted: price is the
  payoff, delta is the payoff's slope with 0 at S = K for a call and -1 at
  S = K for a put (the call convention of `bs_delta` in M5-06 carried
  through put-call parity), gamma, vega and theta are 0. No classical
  derivative exists at the kink and none is claimed.
"""

from __future__ import annotations

import hashlib
import json
import math
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import torch

from svi import SviSlice

YEAR_DAYS = 365.0
DYNAMICS = ("sticky-moneyness", "sticky-strike")


class CounterpartError(Exception):
    """Base of every counterpart failure; never a silent number."""


class InvalidInput(CounterpartError):
    pass


class Unsupported(CounterpartError):
    pass


class CounterpartUnavailable(CounterpartError):
    pass


@dataclass(frozen=True)
class Contract:
    """One European option on one underlying."""

    strike: float
    expiry_days: float
    kind: str  # "call" | "put"
    style: str = "european"

    def check(self) -> None:
        if self.style != "european":
            raise Unsupported(f"exercise style {self.style!r} is not supported")
        if self.kind not in ("call", "put"):
            raise InvalidInput(f"kind must be call or put, got {self.kind!r}")
        if not (math.isfinite(self.strike) and self.strike > 0.0):
            raise InvalidInput(f"strike must be finite and positive, got {self.strike!r}")
        if not (math.isfinite(self.expiry_days) and self.expiry_days >= 0.0):
            raise InvalidInput(f"expiry_days must be finite and non-negative, got {self.expiry_days!r}")


def _sha256_json(obj) -> str:
    return hashlib.sha256(json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _norm_cdf(x: np.ndarray) -> np.ndarray:
    return torch.special.ndtr(torch.from_numpy(np.asarray(x, dtype=np.float64))).numpy()


def _norm_pdf(x: np.ndarray) -> np.ndarray:
    return np.exp(-0.5 * x * x) / math.sqrt(2.0 * math.pi)


# ---------------------------------------------------------------- surfaces


class FlatSurface:
    """A constant implied volatility: w = sigma^2 tau for every k and tau > 0."""

    def __init__(self, sigma: float):
        if not (math.isfinite(sigma) and sigma > 0.0):
            raise InvalidInput(f"sigma must be finite and positive, got {sigma!r}")
        self.sigma = sigma

    def params(self) -> dict:
        return {"kind": "flat", "sigma": self.sigma}

    def vol(self, k, tau):
        """sigma, d sigma / dk, d2 sigma / dk2 and d w / d tau at fixed k."""
        k = np.asarray(k, dtype=float)
        z = np.zeros_like(k)
        return np.full_like(k, self.sigma), z, z, np.full_like(k, self.sigma * self.sigma)


class SviSurface:
    """Raw SVI slices on a grid of expiries, linear in w between them.

    tau outside [first slice, last slice] is InvalidInput: there is no
    extrapolation in tau. Outside the calibrated k range the slices follow
    their own linear SVI wings (a declared extrapolation)."""

    def __init__(self, slices: list[SviSlice]):
        if not slices:
            raise InvalidInput("a surface needs at least one slice")
        taus = [s.tau for s in slices]
        if any(b <= a for a, b in zip(taus, taus[1:])):
            raise InvalidInput("slices must have strictly increasing tau")
        self.slices = slices
        self.taus = np.array(taus)

    def params(self) -> dict:
        return {"kind": "svi", "slices": [s.params() for s in self.slices]}

    def _bracket(self, tau: float):
        lo, hi = self.taus[0], self.taus[-1]
        # A relative tolerance absorbs the rounding of (expiry - as_of) / 365.
        tol = 1e-12 * hi
        if tau < lo - tol or tau > hi + tol:
            raise InvalidInput(f"tau {tau!r} is outside the surface [{lo!r}, {hi!r}]")
        j = int(np.searchsorted(self.taus, tau - tol, side="left"))
        if j < len(self.taus) and abs(self.taus[j] - tau) <= tol:
            # On a node: theta uses the segment towards shorter expiries, the
            # direction calendar time moves in.
            return (max(j - 1, 0), j, 1.0) if j > 0 else (0, 1, 0.0) if len(self.taus) > 1 else (0, 0, 1.0)
        lo_i = j - 1
        x = (tau - self.taus[lo_i]) / (self.taus[j] - self.taus[lo_i])
        return lo_i, j, x

    def total_variance(self, k, tau):
        """w, w_k, w_kk and w_tau at (k, tau)."""
        k = np.asarray(k, dtype=float)
        i, j, x = self._bracket(float(tau))
        a, b = self.slices[i], self.slices[j]
        if i == j:
            return a.w(k), a.w_k(k), a.w_kk(k), np.zeros_like(k)
        dt = b.tau - a.tau
        w = (1.0 - x) * a.w(k) + x * b.w(k)
        w1 = (1.0 - x) * a.w_k(k) + x * b.w_k(k)
        w2 = (1.0 - x) * a.w_kk(k) + x * b.w_kk(k)
        wt = (b.w(k) - a.w(k)) / dt
        return w, w1, w2, wt

    def vol(self, k, tau):
        """sigma, d sigma / dk, d2 sigma / dk2 and d w / d tau at fixed k."""
        w, w1, w2, wt = self.total_variance(k, tau)
        if np.any(w <= 0.0):
            raise InvalidInput("total variance is not positive on the requested k")
        sig = np.sqrt(w / tau)
        # sigma = sqrt(w / tau): d sigma = w_k / (2 tau sigma) dk.
        s1 = w1 / (2.0 * tau * sig)
        s2 = (w2 / (2.0 * tau) - s1 * s1) / sig
        return sig, s1, s2, wt


# ---------------------------------------------------------------- counterparts


class ReferenceCounterpart:
    """Black-Scholes-Merton on a surface, with closed-form total Greeks."""

    name = "reference-bsm"
    version = "1"

    def __init__(self, surface, dynamics: str, s_ref: float, rate: float = 0.0, carry: float = 0.0):
        if dynamics not in DYNAMICS:
            raise InvalidInput(f"dynamics must be one of {DYNAMICS}, got {dynamics!r}")
        for name, v in (("s_ref", s_ref), ("rate", rate), ("carry", carry)):
            if not math.isfinite(v):
                raise InvalidInput(f"{name} must be finite, got {v!r}")
        if s_ref <= 0.0:
            raise InvalidInput(f"s_ref must be positive, got {s_ref!r}")
        self.surface = surface
        self.dynamics = dynamics
        self.s_ref = s_ref
        self.rate = rate
        self.carry = carry

    def config(self) -> dict:
        return {
            "dynamics": self.dynamics,
            "s_ref": self.s_ref,
            "rate": self.rate,
            "carry": self.carry,
            "year_days": YEAR_DAYS,
        }

    def identity(self) -> dict:
        return {
            "name": self.name,
            "version": self.version,
            "implementation_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "config_sha256": _sha256_json(self.config()),
            "dynamics": self.dynamics,
            "surface_sha256": _sha256_json(self.surface.params()),
        }

    # -- input handling

    def _inputs(self, contract: Contract, spot, as_of_days):
        contract.check()
        s = np.asarray(spot, dtype=float)
        if s.dtype == bool or not np.all(np.isfinite(s)) or np.any(s <= 0.0):
            raise InvalidInput("spot must be finite and positive")
        if not (np.isscalar(as_of_days) or np.ndim(as_of_days) == 0):
            raise InvalidInput("as_of_days must be one scalar per call")
        t = float(as_of_days)
        if not math.isfinite(t):
            raise InvalidInput("as_of_days must be finite")
        if t > contract.expiry_days:
            raise InvalidInput(f"as_of_days {t!r} is after expiry {contract.expiry_days!r}")
        tau = (contract.expiry_days - t) / YEAR_DAYS
        return s, tau

    # -- the price function and its derivatives

    def _evaluate(self, contract: Contract, spot, as_of_days, greeks: bool):
        s, tau = self._inputs(contract, spot, as_of_days)
        K = contract.strike
        call = contract.kind == "call"
        if tau == 0.0:
            pay = np.maximum(s - K, 0.0) if call else np.maximum(K - s, 0.0)
            delta = (s > K).astype(float) if call else (s > K).astype(float) - 1.0
            z = np.zeros_like(s)
            return {"price": pay, "delta": delta, "gamma": z, "vega": z, "theta": z}
        r, q = self.rate, self.carry
        df_r, df_q = math.exp(-r * tau), math.exp(-q * tau)
        drift = math.exp((r - q) * tau)
        fwd = s * drift
        k = np.log(K / fwd)
        k_read = k if self.dynamics == "sticky-moneyness" else np.full_like(s, math.log(K / (self.s_ref * drift)))
        sig, s1, s2, w_tau = self.surface.vol(k_read, tau)
        vol = sig * math.sqrt(tau)
        d1 = (np.log(fwd / K) + 0.5 * vol * vol) / vol
        d2 = d1 - vol
        n1 = _norm_pdf(d1)
        if call:
            price = df_r * (fwd * _norm_cdf(d1) - K * _norm_cdf(d2))
            bs_delta = df_q * _norm_cdf(d1)
            black_f = _norm_cdf(d1)
        else:
            price = df_r * (K * _norm_cdf(-d2) - fwd * _norm_cdf(-d1))
            bs_delta = -df_q * _norm_cdf(-d1)
            black_f = _norm_cdf(d1) - 1.0
        if not greeks:
            return {"price": price}
        # Partial derivatives at fixed sigma (vega_s = dV/dsigma).
        vega_s = df_q * s * n1 * math.sqrt(tau)
        gamma_s = df_q * n1 / (s * vol)
        vanna_s = -df_q * n1 * d2 / sig
        volga_s = vega_s * d1 * d2 / sig
        if self.dynamics == "sticky-moneyness":
            # k = ln K - ln S - (r - q) tau: dk/dS = -1/S, d2k/dS2 = 1/S^2.
            ds = -s1 / s
            dss = (s2 + s1) / (s * s)
        else:
            ds = np.zeros_like(s)
            dss = np.zeros_like(s)
        delta = bs_delta + vega_s * ds
        gamma = gamma_s + 2.0 * vanna_s * ds + volga_s * ds * ds + vega_s * dss
        # theta = -dV/dtau. V = e^{-r tau} Black(F(tau), K, w(k(tau), tau)),
        # dk/dtau = -(r - q) for both dynamics; dBlack/dw = K n(d2) / (2 sqrt w).
        black_w = K * _norm_pdf(d2) / (2.0 * vol)
        w_k = 2.0 * tau * sig * s1
        dw_dtau = w_k * (-(r - q)) + w_tau
        dv_dtau = -r * price + df_r * (black_f * fwd * (r - q) + black_w * dw_dtau)
        return {"price": price, "delta": delta, "gamma": gamma, "vega": vega_s, "theta": -dv_dtau}

    def price(self, contract: Contract, spot, as_of_days):
        return self._evaluate(contract, spot, as_of_days, greeks=False)["price"]

    def greeks(self, contract: Contract, spot, as_of_days) -> dict:
        return self._evaluate(contract, spot, as_of_days, greeks=True)

    def surface_params(self) -> dict:
        return self.surface.params()


class MarketMakerRsCounterpart:
    """market-maker-rs as the counterpart; empty until market-maker-rs#95
    (pricing and Greeks) and #96 (volatility surface) exist.

    Proposed contract (experiments/README.md): a market-maker-rs binary at a
    pinned version, run as a batch subprocess on a request CSV, writing a
    response CSV with explicit error rows."""

    name = "market-maker-rs"
    version = None

    def identity(self) -> dict:
        return {"name": self.name, "version": None, "status": "pending market-maker-rs#95/#96"}

    def _unavailable(self, *_args, **_kwargs):
        raise CounterpartUnavailable("market-maker-rs#95/#96: no pricing or surface counterpart yet")

    price = _unavailable
    greeks = _unavailable
    surface_params = _unavailable
