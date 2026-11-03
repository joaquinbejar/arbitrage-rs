"""Conformance suite for pricing counterparts, plus the SVI and Heston checks.

Run from the repository root:

    experiments/.venv/bin/python -m unittest discover -s experiments -p 'test_*.py' -v

Every counterpart runs the same `Conformance` checks; the market-maker-rs
adapter's are skipped until market-maker-rs#95/#96 exist. Tolerances are
declared here, before any run of the experiment, and are not tuned to it.
"""

from __future__ import annotations

import json
import math
import os
import tempfile
import unittest
from pathlib import Path

import numpy as np

import calibration
import counterpart as cp
import heston
import svi
from deep_hedging import CONFIG, bs_call, bs_delta

# ---------------------------------------------------------------- tolerances

PARITY_TOL = 1e-11  # absolute, prices of order 1 to 10 at S = 100
FD_REL_STEP_S = 1e-4  # spot step, relative
FD_STEP_VOL = 1e-4  # volatility step for vega
FD_STEP_DAYS = 1e-3  # one day / 1000 for theta
DELTA_TOL = 1e-5  # absolute
GAMMA_REL_TOL, GAMMA_ABS_TOL = 1e-3, 1e-6
VEGA_REL_TOL, VEGA_ABS_TOL = 1e-6, 1e-6
THETA_REL_TOL, THETA_ABS_TOL = 1e-3, 1e-3  # theta per year
HESTON_XI0_TOL = 1e-4  # absolute, xi = 1e-4 against the xi = 0 limit
HESTON_QUAD_TOL = 1e-10  # absolute change when nodes and cutoff double
HESTON_MC_SE = 4.0  # standard errors, plus the discretisation estimate
SVI_RECOVERY_W_TOL = 1e-10  # total variance


def synthetic_svi_surface() -> cp.SviSurface:
    """A known, arbitrage-free-on-its-grid surface with three daily slices
    and a skew, used where the test needs an SVI surface with no fit."""
    slices = []
    for day in (1, 2, 3):
        tau = day / cp.YEAR_DAYS
        slices.append(svi.SviSlice(tau=tau, a=0.03 * tau, b=0.05 * math.sqrt(tau), rho=-0.6, m=0.01, sigma=0.05))
    return cp.SviSurface(slices)


class ShiftedSurface:
    """The surface of `base` with its implied volatility shifted by `shift`."""

    def __init__(self, base, shift):
        self.base, self.shift = base, shift

    def params(self):
        return {"base": self.base.params(), "shift": self.shift}

    def vol(self, k, tau):
        sig, s1, s2, wt = self.base.vol(k, tau)
        return sig + self.shift, s1, s2, wt


def close(test, got, want, rel, abs_, msg):
    got, want = np.asarray(got), np.asarray(want)
    bad = np.abs(got - want) > abs_ + rel * np.abs(want)
    test.assertFalse(np.any(bad), f"{msg}: got {got[bad][:5]}, want {want[bad][:5]}")


# ---------------------------------------------------------------- conformance


class Conformance:
    """Checks every counterpart must pass. Subclasses give `contracts()`: a
    list of (label, counterpart factory taking a volatility shift, expiry in
    days), `as_of_days(expiry)` and `fd_theta(...)`."""

    def contracts(self):
        raise NotImplementedError

    def test_payoff_at_expiry(self):
        for label, make, expiry in self.contracts():
            c = make(0.0)
            s = np.array([90.0, 100.0, 110.0])
            for kind in ("call", "put"):
                con = cp.Contract(strike=100.0, expiry_days=expiry, kind=kind)
                g = c.greeks(con, s, expiry)
                pay = np.maximum(s - 100.0, 0.0) if kind == "call" else np.maximum(100.0 - s, 0.0)
                np.testing.assert_array_equal(g["price"], pay, err_msg=label)
                want_delta = np.array([0.0, 0.0, 1.0]) if kind == "call" else np.array([-1.0, -1.0, 0.0])
                np.testing.assert_array_equal(g["delta"], want_delta, err_msg=label)
                for key in ("gamma", "vega", "theta"):
                    np.testing.assert_array_equal(g[key], np.zeros(3), err_msg=f"{label} {key}")

    def test_put_call_parity(self):
        for label, make, expiry in self.contracts():
            c = make(0.0)
            s = np.linspace(85.0, 115.0, 31)
            for as_of in self.as_of_days(expiry):
                tau = (expiry - as_of) / cp.YEAR_DAYS
                call = c.price(cp.Contract(100.0, expiry, "call"), s, as_of)
                put = c.price(cp.Contract(100.0, expiry, "put"), s, as_of)
                fwd = s * math.exp((c.rate - c.carry) * tau)
                close(self, call - put, math.exp(-c.rate * tau) * (fwd - 100.0), 0.0, PARITY_TOL, f"{label} parity")

    def test_greeks_against_finite_differences_of_the_same_price(self):
        for label, make, expiry in self.contracts():
            c = make(0.0)
            up, down = make(FD_STEP_VOL), make(-FD_STEP_VOL)
            s = np.array([92.0, 99.0, 100.0, 101.0, 108.0])  # includes the nodal strike S = K
            for kind in ("call", "put"):
                con = cp.Contract(100.0, expiry, kind)
                for as_of in self.as_of_days(expiry):
                    tag = f"{label} {kind} as_of {as_of}"
                    g = c.greeks(con, s, as_of)
                    h = FD_REL_STEP_S * s
                    p_up, p_mid, p_dn = c.price(con, s + h, as_of), c.price(con, s, as_of), c.price(con, s - h, as_of)
                    close(self, g["delta"], (p_up - p_dn) / (2 * h), 0.0, DELTA_TOL, f"{tag} delta")
                    fd_gamma = (p_up - 2 * p_mid + p_dn) / (h * h)
                    close(self, g["gamma"], fd_gamma, GAMMA_REL_TOL, GAMMA_ABS_TOL, f"{tag} gamma")
                    fd_vega = (up.price(con, s, as_of) - down.price(con, s, as_of)) / (2 * FD_STEP_VOL)
                    close(self, g["vega"], fd_vega, VEGA_REL_TOL, VEGA_ABS_TOL, f"{tag} vega")
                    fd_theta = self.fd_theta(c, con, s, as_of) * cp.YEAR_DAYS
                    close(self, g["theta"], fd_theta, THETA_REL_TOL, THETA_ABS_TOL, f"{tag} theta")

    def test_invalid_inputs_are_explicit(self):
        for label, make, expiry in self.contracts():
            c = make(0.0)
            con = cp.Contract(100.0, expiry, "call")
            for bad in (np.array([np.nan]), np.array([-1.0]), np.array([0.0]), np.array([np.inf])):
                with self.assertRaises(cp.InvalidInput, msg=label):
                    c.greeks(con, bad, 0.0)
            for bad in (np.array([True]), [True, False], np.array(["100"])):
                with self.assertRaises(cp.InvalidInput, msg=label):
                    c.greeks(con, bad, 0.0)
            with self.assertRaises(cp.InvalidInput, msg=label):
                c.greeks(con, np.array([100.0]), True)
            with self.assertRaises(cp.InvalidInput, msg=label):
                c.greeks(con, np.array([100.0]), expiry + 1.0)
            with self.assertRaises(cp.InvalidInput, msg=label):
                c.greeks(con, np.array([100.0]), np.array([0.0, 1.0]))
            with self.assertRaises(cp.Unsupported, msg=label):
                c.greeks(cp.Contract(100.0, expiry, "call", style="american"), np.array([100.0]), 0.0)
            with self.assertRaises(cp.InvalidInput, msg=label):
                c.greeks(cp.Contract(100.0, expiry, "straddle"), np.array([100.0]), 0.0)
            with self.assertRaises(cp.InvalidInput, msg=label):
                c.greeks(cp.Contract(-100.0, expiry, "call"), np.array([100.0]), 0.0)

    def test_identity_names_implementation_configuration_and_surface(self):
        for label, make, _ in self.contracts():
            ident = make(0.0).identity()
            for key in ("name", "version", "implementation_sha256", "config_sha256", "dynamics", "surface_sha256"):
                self.assertIn(key, ident, label)
            self.assertNotEqual(make(0.0).identity()["surface_sha256"], make(0.01).identity()["surface_sha256"])


class ReferenceConformance(Conformance, unittest.TestCase):
    def contracts(self):
        out = []
        for dyn in cp.DYNAMICS:
            out.append(
                (
                    f"flat r,q {dyn}",
                    lambda shift, dyn=dyn: cp.ReferenceCounterpart(
                        ShiftedSurface(cp.FlatSurface(0.2), shift) if shift else cp.FlatSurface(0.2),
                        dyn,
                        s_ref=100.0,
                        rate=0.03,
                        carry=0.01,
                    ),
                    30.0,
                )
            )
            out.append(
                (
                    f"svi {dyn}",
                    lambda shift, dyn=dyn: cp.ReferenceCounterpart(
                        ShiftedSurface(synthetic_svi_surface(), shift) if shift else synthetic_svi_surface(),
                        dyn,
                        s_ref=100.0,
                    ),
                    3.0,
                )
            )
        return out

    def as_of_days(self, expiry):
        # On a node, between nodes, and at tau = one day (the first node of
        # the SVI surface).
        return [0.0, expiry - 1.5, expiry - 1.0]

    def fd_theta(self, c, con, s, as_of):
        """Per day. Between nodes: central. On a node: one-sided towards
        shorter expiries (the segment theta uses), except at the first node
        of a surface, whose only segment is towards longer expiries."""
        h = FD_STEP_DAYS
        tau_days = con.expiry_days - as_of
        on_node = abs(tau_days - round(tau_days)) < 1e-12
        surface = c.surface.base if isinstance(c.surface, ShiftedSurface) else c.surface
        first = isinstance(surface, cp.SviSurface) and round(tau_days) == 1
        if not on_node or isinstance(surface, cp.FlatSurface):
            return (c.price(con, s, as_of + h) - c.price(con, s, as_of - h)) / (2 * h)
        if first:
            return (c.price(con, s, as_of) - c.price(con, s, as_of - h)) / h
        return (c.price(con, s, as_of + h) - c.price(con, s, as_of)) / h

    def test_flat_surface_delta_is_the_legacy_function_bit_for_bit(self):
        c = cp.ReferenceCounterpart(cp.FlatSurface(CONFIG["sigma"]), "sticky-moneyness", s_ref=CONFIG["s0"])
        con = cp.Contract(CONFIG["strike"], float(CONFIG["steps"]), "call")
        s = np.random.default_rng(3).uniform(70.0, 130.0, 4096)
        for t in range(CONFIG["steps"] + 1):
            tau = (CONFIG["steps"] - t) / CONFIG["days_per_year"]
            np.testing.assert_array_equal(c.greeks(con, s, float(t))["delta"], bs_delta(s, tau, CONFIG))
        premium = float(c.price(con, np.array([CONFIG["s0"]]), 0.0)[0])
        legacy = bs_call(CONFIG["s0"], CONFIG["steps"] / CONFIG["days_per_year"], CONFIG)
        self.assertLessEqual(abs(premium - legacy), 1e-13 * legacy)

    def test_tau_zero_with_spot_at_strike_is_resolved_before_the_surface(self):
        c = cp.ReferenceCounterpart(synthetic_svi_surface(), "sticky-moneyness", s_ref=100.0)
        g = c.greeks(cp.Contract(100.0, 3.0, "call"), np.array([100.0]), 3.0)
        self.assertEqual((g["price"][0], g["delta"][0], g["gamma"][0]), (0.0, 0.0, 0.0))

    def test_svi_surface_rejects_tau_outside_its_slices(self):
        c = cp.ReferenceCounterpart(synthetic_svi_surface(), "sticky-strike", s_ref=100.0)
        for as_of in (2.5, 0.0 - 1.0):  # tau = half a day; tau = 4 days
            with self.assertRaises(cp.InvalidInput):
                c.greeks(cp.Contract(100.0, 3.0, "call"), np.array([100.0]), as_of)

    def test_sticky_strike_delta_is_the_bs_delta_at_the_smile_vol(self):
        surf = synthetic_svi_surface()
        c = cp.ReferenceCounterpart(surf, "sticky-strike", s_ref=100.0)
        s = np.array([95.0, 105.0])
        g = c.greeks(cp.Contract(102.0, 3.0, "call"), s, 1.0)
        sig = surf.vol(np.full(2, math.log(102.0 / 100.0)), 2.0 / cp.YEAR_DAYS)[0]
        vol = sig * math.sqrt(2.0 / cp.YEAR_DAYS)
        d1 = (np.log(s / 102.0) + 0.5 * vol * vol) / vol
        np.testing.assert_allclose(g["delta"], cp._norm_cdf(d1), rtol=0, atol=1e-15)




def flat_as_svi_json(sigma: float, days: int) -> str:
    """A flat surface as SVI slices with b = 0 (w = sigma^2 tau, one slice a
    day) and the constant-vol policy below the first day: exactly the
    reference's FlatSurface on (0, days]."""
    slices = [
        svi.SviSlice(tau=d / cp.YEAR_DAYS, a=sigma * sigma * d / cp.YEAR_DAYS, b=0.0, rho=0.0, m=0.0, sigma=0.1)
        for d in range(1, days + 1)
    ]
    return cp.svi_surface_json(slices, [(-10.0, 10.0)] * len(slices), short_expiry="constant-vol")


def synthetic_svi_json() -> str:
    surf = synthetic_svi_surface()
    return cp.svi_surface_json(surf.slices, [(-10.0, 10.0)] * len(surf.slices))


def mm_available() -> str | None:
    """Why the market-maker-rs counterpart cannot run here, or None."""
    try:
        cp.mm_identity(Path(os.environ.get(cp.MM_BINARY_ENV, "")))
    except cp.CounterpartError as e:
        return str(e)
    return None


class MarketMakerRsAdapter(unittest.TestCase):
    """What the adapter guarantees without a binary."""

    def test_no_binary_is_unavailable_and_never_a_number(self):
        with self.assertRaises(cp.CounterpartUnavailable):
            cp.MarketMakerRsCounterpart(synthetic_svi_json(), "sticky-moneyness", 100.0, binary="/nonexistent/counterpart")

    def test_a_binary_that_is_not_the_manifest_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "counterpart"
            fake.write_text("#!/bin/sh\necho '{}'\n")
            fake.chmod(0o755)
            with self.assertRaises(cp.CounterpartUnavailable):
                cp.MarketMakerRsCounterpart(synthetic_svi_json(), "sticky-moneyness", 100.0, binary=str(fake))

    def test_surface_files_round_trip_bit_for_bit(self):
        surf = synthetic_svi_surface()
        slices, ranges = cp.slices_from_surface_json(synthetic_svi_json())
        self.assertEqual([sl.params() for sl in slices], [sl.params() for sl in surf.slices])
        self.assertEqual(ranges, [(-10.0, 10.0)] * 3)

    def test_a_non_finite_or_invalid_response_is_a_protocol_failure(self):
        """A fake binary answers one bad number in each column: NaN, +-inf
        or text. None of them may become a value."""
        cols = ("price", "delta", "gamma", "vega", "theta", "total_variance")
        for col in cols:
            for bad, why in (("NaN", "not finite"), ("inf", "not finite"), ("-inf", "not finite"), ("abc", "not a number")):
                with self.subTest(col=col, bad=bad), tempfile.TemporaryDirectory() as tmp:
                    d = Path(tmp)
                    cells = {c: "0.5" for c in cols} | {col: bad}
                    row = ["0", "ok", *[cells[c] for c in cols], "sticky-moneyness", ""]
                    responses = d / "responses.csv"
                    responses.write_text(",".join(cp.MM_RESPONSE_COLUMNS) + "\n" + ",".join(row) + "\n")
                    fake = d / "counterpart"
                    fake.write_text(f"#!/bin/sh\ncp {responses} \"$(eval echo \\${{$#}})\"\n")
                    fake.chmod(0o755)
                    c = cp.MarketMakerRsCounterpart.__new__(cp.MarketMakerRsCounterpart)
                    c.binary, c.surface_json, c.dynamics = fake, synthetic_svi_json(), "sticky-moneyness"
                    c.s_ref, c.rate, c.carry, c.vol_shift, c._cache = 100.0, 0.0, 0.0, 0.0, {}
                    with self.assertRaisesRegex(cp.CounterpartUnavailable, f"{col} '{bad}' is {why}"):
                        c.greeks(cp.Contract(100.0, 3.0, "call"), np.array([100.0]), 0.0)

    def test_plain_decimals_have_no_exponent(self):
        for x, want in ((1e-5, "0.00001"), (100.0, "100.0"), (1e22, "10000000000000000000000"), (0.1, "0.1")):
            self.assertEqual(cp.plain_decimal(x), want)


@unittest.skipIf(mm_available() is not None, f"market-maker-rs counterpart: {mm_available()}")
class MarketMakerRsConformance(Conformance, unittest.TestCase):
    """The conformance suite, unchanged, on the market-maker-rs binary that
    the manifest pins (set MM_COUNTERPART_BIN), on the reference's surfaces."""

    def contracts(self):
        out = []
        for dyn in cp.DYNAMICS:
            out.append(
                (
                    f"flat r,q {dyn}",
                    lambda shift, dyn=dyn: cp.MarketMakerRsCounterpart(
                        flat_as_svi_json(0.2, 31), dyn, s_ref=100.0, rate=0.03, carry=0.01, vol_shift=shift
                    ),
                    30.0,
                )
            )
            out.append(
                (
                    f"svi {dyn}",
                    lambda shift, dyn=dyn: cp.MarketMakerRsCounterpart(
                        synthetic_svi_json(), dyn, s_ref=100.0, vol_shift=shift
                    ),
                    3.0,
                )
            )
        return out

    def as_of_days(self, expiry):
        return [0.0, expiry - 1.5, expiry - 1.0]

    def fd_theta(self, c, con, s, as_of):
        """As the reference's: the flat surface is linear in tau everywhere
        (central); on an SVI node, the segment towards shorter expiries, or
        the first node's only segment."""
        h = FD_STEP_DAYS
        tau_days = con.expiry_days - as_of
        on_node = abs(tau_days - round(tau_days)) < 1e-12
        flat = all(sl.b == 0.0 for sl in c.slices)
        if not on_node or flat:
            return (c.price(con, s, as_of + h) - c.price(con, s, as_of - h)) / (2 * h)
        if round(tau_days) == 1:
            return (c.price(con, s, as_of) - c.price(con, s, as_of - h)) / h
        return (c.price(con, s, as_of + h) - c.price(con, s, as_of)) / h

    def test_agrees_with_the_reference_on_the_same_surface(self):
        s = np.array([80.0, 92.0, 99.0, 100.0, 101.0, 108.0, 125.0])
        cases = [
            ("flat", flat_as_svi_json(0.2, 31), cp.FlatSurface(0.2), 30.0, (0.0, 14.5, 29.0), 0.03, 0.01),
            ("svi", synthetic_svi_json(), synthetic_svi_surface(), 3.0, (0.0, 1.5, 2.0), 0.0, 0.0),
        ]
        for label, text, ref_surface, expiry, as_ofs, r, q in cases:
            for dyn in cp.DYNAMICS:
                mm = cp.MarketMakerRsCounterpart(text, dyn, s_ref=100.0, rate=r, carry=q)
                ref = cp.ReferenceCounterpart(ref_surface, dyn, s_ref=100.0, rate=r, carry=q)
                for kind in ("call", "put"):
                    con = cp.Contract(100.0, expiry, kind)
                    for as_of in as_ofs:
                        got, want = mm.greeks(con, s, as_of), ref.greeks(con, s, as_of)
                        for k, (abs_tol, rel_tol) in cp.AGREEMENT_TOLERANCES.items():
                            bound = np.maximum(abs_tol, rel_tol * np.abs(want[k]))
                            bad = np.abs(got[k] - want[k]) > bound
                            self.assertFalse(
                                np.any(bad),
                                f"{label} {dyn} {kind} as_of {as_of} {k}: {got[k][bad][:3]} vs {want[k][bad][:3]}",
                            )

    def test_error_classes_are_kept_apart(self):
        c = cp.MarketMakerRsCounterpart(synthetic_svi_json(), "sticky-moneyness", s_ref=100.0)
        with self.assertRaises(cp.OutOfDomain):  # tau = 4 days, after the last slice
            c.greeks(cp.Contract(100.0, 4.0, "call"), np.array([100.0]), 0.0)
        hot = cp.MarketMakerRsCounterpart(synthetic_svi_json(), "sticky-moneyness", s_ref=100.0, rate=1e5)
        with self.assertRaises(cp.NumericalFailure):  # e^{-r tau} underflows
            hot.greeks(cp.Contract(100.0, 3.0, "call"), np.array([100.0]), 0.0)

    def test_wings_price_alike_on_both_sides(self):
        """Three points a day outside each slice's k range of surface B (the
        frozen run surface), priced by the binary and by the reference on
        the same slices, within the agreement tolerances."""
        path = Path(__file__).resolve().parent.parent / "research" / "results" / "deep_hedging_mm_surface_b.json"
        text = path.read_text()
        slices, ranges = cp.slices_from_surface_json(text)
        mm = cp.MarketMakerRsCounterpart(text, "sticky-moneyness", s_ref=100.0)
        ref = cp.ReferenceCounterpart(cp.SviSurface(slices), "sticky-moneyness", s_ref=100.0)
        for day, (lo, hi) in enumerate(ranges, start=1):
            con = cp.Contract(100.0, float(day), "call")
            # k = ln(K / S): spots that put k below, above and far above the range.
            s = 100.0 * np.exp(-np.array([lo - 0.02, hi + 0.02, hi + 0.2]))
            got, want = mm.greeks(con, s, 0.0), ref.greeks(con, s, 0.0)
            for k, (abs_tol, rel_tol) in cp.AGREEMENT_TOLERANCES.items():
                bound = np.maximum(abs_tol, rel_tol * np.abs(want[k]))
                self.assertTrue(np.all(np.abs(got[k] - want[k]) <= bound), f"day {day} {k}: {got[k]} vs {want[k]}")

    def test_a_slice_one_ulp_outside_the_svi_domain_is_not_representable(self):
        import deep_hedging_mm

        good = svi.SviSlice(tau=1 / cp.YEAR_DAYS, a=1e-4, b=0.01, rho=-1.0, m=0.0, sigma=0.05)
        bad = svi.SviSlice(tau=1 / cp.YEAR_DAYS, a=1e-4, b=0.01, rho=math.nextafter(-1.0, -2.0), m=0.0, sigma=0.05)
        binary = Path(os.environ[cp.MM_BINARY_ENV])
        out = deep_hedging_mm.representability(binary, [good, bad], [(-0.1, 0.1)] * 2)
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["params"]["rho"], "-1.0000000000000002")
        self.assertIn("|rho| must be at most 1", out[0]["error"])
        self.assertEqual((out[0]["reason"], out[0]["rho_bits"]), ("rho outside [-1, 1]", "bff0000000000001"))
        # Any other refusal is not caught: a negative b aborts.
        other = svi.SviSlice(tau=1 / cp.YEAR_DAYS, a=1e-4, b=-0.01, rho=0.0, m=0.0, sigma=0.05)
        with self.assertRaises(cp.CounterpartUnavailable):
            deep_hedging_mm.representability(binary, [other], [(-0.1, 0.1)])

    def test_sticky_strike_and_sticky_moneyness_read_different_points(self):
        con = cp.Contract(100.0, 3.0, "put")
        s = np.array([95.0, 105.0])
        sm = cp.MarketMakerRsCounterpart(synthetic_svi_json(), "sticky-moneyness", s_ref=100.0)
        ss = cp.MarketMakerRsCounterpart(synthetic_svi_json(), "sticky-strike", s_ref=100.0)
        w_ss = ss.total_variance(con, s, 0.0)
        self.assertEqual(w_ss[0], w_ss[1])
        self.assertNotEqual(sm.total_variance(con, s, 0.0)[0], sm.total_variance(con, s, 0.0)[1])
        self.assertFalse(np.array_equal(sm.greeks(con, s, 0.0)["delta"], ss.greeks(con, s, 0.0)["delta"]))


# ---------------------------------------------------------------- SVI


class SviFit(unittest.TestCase):
    FIT = {"grid": 21, "refinements": 12, "sigma_lo": 0.005, "sigma_hi": 2.0, "max_recentres": 40}

    def test_fit_recovers_a_known_slice(self):
        tau = 10.0 / cp.YEAR_DAYS
        true = svi.SviSlice(tau=tau, a=0.0008, b=0.004, rho=-0.5, m=0.01, sigma=0.03)
        k = np.linspace(-0.25, 0.25, 41)
        w = true.w(k)
        got, rep = svi.fit_slice(tau, k, w, np.ones_like(k), self.FIT)
        self.assertLessEqual(float(np.max(np.abs(got.w(k) - w))), SVI_RECOVERY_W_TOL)
        self.assertEqual(rep["active_constraints"], [])

    def test_box_least_squares_with_an_active_constraint_is_the_constrained_minimum(self):
        # Data from rho = -1.3 (d < -c): the unconstrained minimiser violates
        # |d| <= c, so the solution must sit on that face.
        rng = np.random.default_rng(5)
        y = np.linspace(-3.0, 3.0, 25)
        design = np.stack([np.ones_like(y), y, np.sqrt(y * y + 1.0)], axis=1)
        target = design @ np.array([0.01, -0.013, 0.01])
        g, h = svi._constraints(sigma=1.0, a_max=1.0)
        x, active, obj = svi.solve_box_ls(design, target, np.ones_like(y), g, h)
        self.assertIn(2, active)  # -d - c <= 0, i.e. d >= -c
        self.assertAlmostEqual(x[1], -x[2], places=15)
        # No feasible point of a dense random sample does better.
        pts = rng.uniform([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], (200000, 3))
        pts = pts[np.all(pts @ g.T <= h, axis=1)]
        objs = np.sum((pts @ design.T - target) ** 2, axis=1)
        self.assertLessEqual(obj, float(np.min(objs)))

    def test_diagnostics_flag_calendar_and_butterfly_failures(self):
        cfg = {"heston": {"v0": 0.04}, "k_std": [-4.0, 4.0], "diagnostic_points": 201}
        tau1, tau2 = 1.0 / cp.YEAR_DAYS, 2.0 / cp.YEAR_DAYS
        ok = svi.SviSlice(tau=tau1, a=0.0001, b=0.001, rho=0.0, m=0.0, sigma=0.01)
        lower = svi.SviSlice(tau=tau2, a=0.00005, b=0.001, rho=0.0, m=0.0, sigma=0.01)
        out = calibration.diagnose([ok, lower], {}, cfg)
        self.assertTrue(any("calendar" in f for f in out["failures"]))
        steep = svi.SviSlice(tau=tau1, a=0.00001, b=1.9, rho=0.9, m=0.0, sigma=0.001)
        out = calibration.diagnose([steep], {}, cfg)
        self.assertTrue(any("butterfly" in f or "Lee" in f for f in out["failures"]))


# ---------------------------------------------------------------- weights


class FrozenWeights(unittest.TestCase):
    def test_weights_round_trip_with_a_stable_hash_and_reject_a_mismatch(self):
        import tempfile
        from pathlib import Path

        import torch

        from deep_hedging import Net
        from deep_hedging_counterpart import load_weights, sha256_bytes, weights_bytes

        torch.manual_seed(1)
        net = Net(CONFIG["hidden"], CONFIG["holding_bound"]).double()
        blob = weights_bytes(net)
        self.assertEqual(blob, weights_bytes(net))
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "w.npz"
            path.write_bytes(blob)
            back = load_weights(path, sha256_bytes(blob), CONFIG)
            x = torch.from_numpy(np.random.default_rng(2).normal(size=(64, 3)))
            with torch.no_grad():
                self.assertTrue(torch.equal(net(x), back(x)))
            with self.assertRaises(ValueError):
                load_weights(path, "0" * 64, CONFIG)


# ---------------------------------------------------------------- Heston


class HestonPricer(unittest.TestCase):
    QUAD = {"nodes": 32, "cutoff": 40.0}

    @staticmethod
    def bs(spot, strikes, w):
        vol = math.sqrt(w)
        d1 = (np.log(spot / strikes) + 0.5 * w) / vol
        return spot * cp._norm_cdf(d1) - strikes * cp._norm_cdf(d1 - vol)

    def test_small_vol_of_vol_matches_the_deterministic_variance_limit(self):
        strikes = np.array([90.0, 95.0, 100.0, 105.0, 110.0])
        for v0 in (0.04, 0.09):  # v0 = theta, and v0 != theta
            p = {"kappa": 1.0, "theta": 0.04, "xi": 1e-4, "rho": -0.7, "v0": v0}
            for tau in (1.0 / 365.0, 30.0 / 365.0):
                got = heston.call_price(100.0, strikes, tau, p, self.QUAD)
                want = self.bs(100.0, strikes, heston.integrated_variance(tau, p))
                close(self, got, want, 0.0, HESTON_XI0_TOL, f"v0 {v0} tau {tau}")

    def test_quadrature_converges(self):
        p = CONFIG["heston"] | {"v0": CONFIG["heston"]["v0"]}
        for tau in (1.0 / 365.0, 30.0 / 365.0):
            strikes = 100.0 * np.exp(np.linspace(-4.0, 4.0, 33) * math.sqrt(0.04 * tau))
            _, change = heston.price_converged(100.0, strikes, tau, p, self.QUAD)
            self.assertLessEqual(change, HESTON_QUAD_TOL)

    def test_monte_carlo_agrees_within_error_and_discretisation(self):
        p = {k: CONFIG["heston"][k] for k in ("v0", "kappa", "theta", "xi", "rho")}
        for tau in (1.0 / 365.0, 30.0 / 365.0):
            strikes = 100.0 * np.exp(np.array([-2.0, 0.0, 2.0]) * math.sqrt(0.04 * tau))
            cf = heston.call_price(100.0, strikes, tau, p, self.QUAD)
            coarse, _ = heston.mc_call(100.0, strikes, tau, p, 2**17, 10, 365.0, np.random.default_rng(11))
            fine, se = heston.mc_call(100.0, strikes, tau, p, 2**17, 40, 365.0, np.random.default_rng(12))
            bound = HESTON_MC_SE * se + np.abs(fine - coarse)
            self.assertTrue(np.all(np.abs(fine - cf) <= bound), f"tau {tau}: {fine - cf} vs {bound}")


if __name__ == "__main__":
    unittest.main()
