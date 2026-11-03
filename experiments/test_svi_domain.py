"""The fitted SVI parameters stay in the raw SVI domain: rho is recovered
from an active bound exactly, and a fit outside the domain is an error."""

from __future__ import annotations

import json
import math
import unittest
from pathlib import Path

import numpy as np

import svi

FIT = {"grid": 21, "refinements": 12, "sigma_lo": 0.005, "sigma_hi": 2.0, "max_recentres": 40}
RESULTS = Path(__file__).resolve().parent.parent / "research" / "results"


def fit(rho: float, b: float = 0.05, a: float = 0.01):
    tau = 30 / 365
    truth = svi.SviSlice(tau=tau, a=a, b=b, rho=rho, m=0.01, sigma=0.08)
    k = np.linspace(-0.3, 0.3, 25)
    w = truth.w(k)
    return svi.fit_slice(tau, k, w, np.ones_like(k), FIT)


class RecoverRho(unittest.TestCase):
    sigma, a_max = 0.1, 1.0

    def gh(self):
        return svi._constraints(self.sigma, self.a_max)

    def test_an_active_rho_bound_gives_rho_exactly(self):
        g, h = self.gh()
        c = 0.003
        for d, want, row in ((c, 1.0, svi.RHO_UPPER), (-c, -1.0, svi.RHO_LOWER)):
            # d / c rounded one ulp outside: the face says exactly +-1.
            x = np.array([0.01, d * (1 + 2e-16), c])
            self.assertEqual(svi.recover_rho(x, (row,), g, h), want)

    def test_near_a_bound_but_outside_the_tolerance_is_not_snapped(self):
        g, h = self.gh()
        c = 0.003
        x = np.array([0.01, -c * (1 - 1e-6), c])
        self.assertEqual(svi.recover_rho(x, (), g, h), x[1] / x[2])

    def test_inside_the_tolerance_is_snapped_and_moves_w_by_its_stated_bound(self):
        # Snapping changes w by (c - |d|) |k - m| / sigma exactly: check it
        # on a spread of sigma, k and gaps inside the tolerance.
        rng = np.random.default_rng(5)
        for _ in range(200):
            sigma = float(10 ** rng.uniform(-3, 0))
            g, h = svi._constraints(sigma, self.a_max)
            scale = max(1.0, float(np.max(np.abs(h))))
            c = float(10 ** rng.uniform(-6, -1))
            gap = float(rng.uniform(0, 1)) * svi.FEASIBILITY_TOLERANCE * scale
            sign = rng.choice([-1.0, 1.0])
            x = np.array([0.01, sign * (c - gap), c])
            rho = svi.recover_rho(x, (), g, h)
            self.assertEqual(rho, sign)
            k = np.linspace(-2.0, 2.0, 41)
            m = 0.0
            b = c / sigma
            snapped = svi.SviSlice(tau=0.1, a=0.01, b=b, rho=rho, m=m, sigma=sigma).w(k)
            raw = svi.SviSlice(tau=0.1, a=0.01, b=b, rho=x[1] / x[2], m=m, sigma=sigma).w(k)
            bound = (c - abs(x[1])) * np.abs(k - m) / sigma
            # The term that changes, b (rho - d / c) (k - m), is the stated
            # bound up to the rounding of d / c (2 eps in rho); the full w
            # differs by it plus the rounding of evaluating w twice.
            term = np.abs(b * (rho - x[1] / x[2]) * (k - m))
            eps = np.finfo(float).eps
            slack = 2 * eps * b * np.abs(k - m)
            self.assertTrue(np.all(term <= bound + slack))
            noise = 4 * eps * np.maximum(np.abs(raw), np.abs(snapped))
            self.assertTrue(np.all(np.abs(snapped - raw) <= bound + slack + noise))

    def test_interior_rho_is_d_over_c(self):
        g, h = self.gh()
        x = np.array([0.01, -0.0015, 0.003])
        self.assertEqual(svi.recover_rho(x, (), g, h), -0.5)

    def test_only_exactly_zero_c_is_flat(self):
        g, h = self.gh()
        self.assertEqual(svi.recover_rho(np.array([0.01, 0.0, 0.0]), (), g, h), 0.0)
        # A tiny positive c on the upper face is not flat: rho = 1.
        self.assertEqual(svi.recover_rho(np.array([0.01, 9e-13, 9e-13]), (), g, h), 1.0)
        # Both bounds within the tolerance: rho is not identified, d / c kept.
        self.assertEqual(svi.recover_rho(np.array([0.01, 1e-13, 4e-13]), (), g, h), 0.25)

    def test_non_finite_solutions_are_errors(self):
        g, h = self.gh()
        for bad in (math.nan, math.inf):
            with self.assertRaises(ValueError):
                svi.recover_rho(np.array([0.01, bad, 0.003]), (), g, h)


class DomainCheck(unittest.TestCase):
    def slice(self, **kw):
        p = {"tau": 0.1, "a": 0.01, "b": 0.05, "rho": -0.5, "m": 0.0, "sigma": 0.1} | kw
        return svi.SviSlice(**p)

    def test_every_violation_is_an_error_and_nan_fails(self):
        for kw in (
            {"rho": math.nextafter(-1.0, -2.0)},
            {"rho": math.nan},
            {"b": -1e-12},
            {"b": 1.5, "rho": 0.5},
            {"sigma": 0.0},
            {"a": -1e-18},
            {"a": 2.0},
        ):
            with self.subTest(**kw), self.assertRaises(ValueError):
                svi.check_slice_domain(self.slice(**kw), a_max=1.0)
        svi.check_slice_domain(self.slice(rho=-1.0), a_max=1.0)


class Fits(unittest.TestCase):
    def test_a_fit_on_a_rho_bound_recovers_it_exactly(self):
        for rho in (-1.0, 1.0):
            sl, rep = fit(rho)
            self.assertLessEqual(abs(sl.rho), 1.0)
            self.assertEqual(sl.rho, rho, rep)

    def test_an_interior_fit_keeps_rho_inside(self):
        sl, _ = fit(-0.4)
        self.assertLess(abs(sl.rho), 1.0)
        self.assertAlmostEqual(sl.rho, -0.4, places=6)

    def test_the_published_counterpart_fit_is_reproduced_with_rho_in_the_domain(self):
        # The surface of research/results/deep_hedging_counterpart.json,
        # refitted. Days 1 to 16 sit on the rho >= -1 face (the solver
        # reports it active); their stored rho is d / c, up to four ulp from
        # -1 and on five days one ulp below it. They now give -1.0 exactly.
        # Every other parameter of every slice, and rho on the other days,
        # is unchanged bit for bit.
        import calibration
        from deep_hedging_counterpart import COUNTERPART_CONFIG

        stored = json.loads((RESULTS / "deep_hedging_counterpart.json").read_text())["calibration"]["slices"]
        slices, _ = calibration.calibrate(COUNTERPART_CONFIG)
        outside = [s["day"] for s in stored if abs(s["params"]["rho"]) > 1.0]
        self.assertEqual(outside, [1, 4, 9, 10, 13])
        for got, rec in zip(slices, stored, strict=True):
            want = rec["params"]
            for key in ("tau", "a", "b", "m", "sigma"):
                self.assertEqual(getattr(got, key), want[key], (rec["day"], key))
            if svi.RHO_LOWER in rec["fit"]["active_constraints"]:
                self.assertEqual(got.rho, -1.0, rec["day"])
            else:
                self.assertEqual(got.rho, want["rho"], rec["day"])


if __name__ == "__main__":
    unittest.main()
