"""Checks of the #43 run's comparison that need no binary."""

from __future__ import annotations

import math
import unittest

import numpy as np

import counterpart as cp
import deep_hedging_mm as m

KEYS = ("price", "delta", "gamma", "vega", "theta")
CFG = {"steps": 1, "strike": 100.0, "s0": 100.0}
COV = {"by_day": {1: {"k_range": [-0.1, 0.1]}}}


class Fixed:
    """A counterpart that answers fixed outputs."""

    def __init__(self, values: dict, dynamics: str = "sticky-moneyness"):
        self.values, self.dynamics = values, dynamics

    def greeks(self, contract, s, as_of):
        return {k: np.array(v, dtype=float) for k, v in self.values.items()}


def ones(**over):
    v = {k: [1.0] for k in KEYS}
    v.update(over)
    return v


class Compare(unittest.TestCase):
    con = cp.Contract(100.0, 1.0, "call")
    s = np.array([[100.0]])

    def test_equal_outputs_agree_in_every_panel(self):
        r = m.compare(Fixed(ones()), Fixed(ones()), self.con, self.s, COV, CFG)
        self.assertTrue(all(r["agrees"].values()))
        self.assertEqual(r["all"]["price"]["violations"], 0)

    def test_a_non_finite_output_on_either_side_fails_the_run(self):
        for bad in (math.nan, math.inf, -math.inf):
            for key in KEYS:
                for sides in ((ones(**{key: [bad]}), ones()), (ones(), ones(**{key: [bad]}))):
                    with self.subTest(bad=bad, key=key, mm_side=sides[0] is not None):
                        with self.assertRaises(cp.NumericalFailure):
                            m.compare(Fixed(sides[0]), Fixed(sides[1]), self.con, self.s, COV, CFG)

    def test_a_missing_or_misshapen_output_fails_the_run(self):
        missing = ones()
        del missing["vega"]
        for mm in (missing, ones(price=[1.0, 2.0])):
            with self.assertRaises(cp.NumericalFailure):
                m.compare(Fixed(mm), Fixed(ones()), self.con, self.s, COV, CFG)

    def test_a_difference_beyond_the_tolerance_is_counted_with_its_row(self):
        r = m.compare(Fixed(ones(delta=[1.5])), Fixed(ones()), self.con, self.s, COV, CFG)
        self.assertFalse(r["agrees"]["all"])
        self.assertEqual(r["all"]["delta"]["violations"], 1)
        self.assertEqual(r["all"]["delta"]["max_ratio_at"]["path"], 0)
        self.assertGreater(r["all"]["delta"]["max_ratio"], 1.0)


if __name__ == "__main__":
    unittest.main()


def learned(seed, **over):
    row = {"seed": seed, "cvar95": 1.25, "mean_pnl": -0.5, "trades": 12, "delta_cvar95_vs_band": {"estimate": -0.1, "ci_low": -0.2, "ci_high": -0.01}}
    row.update(over)
    return row


def by_eps(rows=None, **entry):
    rows = rows if rows is not None else [learned(11), learned(12)]
    e = {"band_w": 0.01, "in_model": {"delta": {"cvar95": 2.0}, "band": {"cvar95": 1.5}, "learned": rows}}
    e.update(entry)
    return {"0.0005": e}


class InModelComparison(unittest.TestCase):
    def test_metadata_alone_is_equal_and_counted_per_side(self):
        new = by_eps([learned(11, weights_sha256="aa"), learned(12, weights_sha256="bb")])
        r = m.compare_in_model(new, by_eps())
        self.assertTrue(r["equal"], r)
        self.assertEqual((r["rows"], r["shared_fields"], r["metadata_fields"]), (2, 10, {"new": 2, "old": 0}))

    def test_weights_sha256_is_checked_against_the_weight_record(self):
        new = by_eps([learned(11, weights_sha256="aa"), learned(12, weights_sha256="bb")])
        good = {"0.0005/11": {"sha256": "aa"}, "0.0005/12": {"sha256": "bb"}}
        self.assertTrue(m.compare_in_model(new, by_eps(), weights=good)["equal"])
        bad = good | {"0.0005/12": {"sha256": "cc"}}
        r = m.compare_in_model(new, by_eps(), weights=bad)
        self.assertFalse(r["equal"])
        self.assertEqual(len(r["identity_errors"]), 1)

    def test_rows_are_matched_by_seed_not_position(self):
        r = m.compare_in_model(by_eps([learned(12), learned(11)]), by_eps())
        self.assertTrue(r["equal"], r)

    def test_every_kind_of_difference_fails(self):
        cases = {
            "a differing value": by_eps([learned(11, cvar95=1.26), learned(12)]),
            "a missing field": by_eps([{k: v for k, v in learned(11).items() if k != "trades"}, learned(12)]),
            "an unknown extra field": by_eps([learned(11, note="x"), learned(12)]),
            "bool for int": by_eps([learned(11, trades=True), learned(12)]),
            "int for float": by_eps([learned(11, cvar95=1), learned(12)]),
            "a non-finite value": by_eps([learned(11, cvar95=math.nan), learned(12)]),
            "infinite": by_eps([learned(11, mean_pnl=-math.inf), learned(12)]),
            "fewer rows": by_eps([learned(11)]),
            "a duplicated seed": by_eps([learned(11), learned(11)]),
            "another seed": by_eps([learned(11), learned(13)]),
            "band_w": by_eps(band_w=0.02),
            "negative zero": by_eps([learned(11, mean_pnl=-0.0), learned(12)]),
            "a nested value": by_eps([learned(11, delta_cvar95_vs_band={"estimate": -0.1, "ci_low": -0.2, "ci_high": 0.0}), learned(12)]),
        }
        for name, new in cases.items():
            with self.subTest(name):
                self.assertFalse(m.compare_in_model(new, by_eps())["equal"])
        # The reference side: a duplicated seed, a seed only there, and zero
        # against negative zero.
        self.assertFalse(m.compare_in_model(by_eps(), by_eps([learned(11), learned(11)]))["equal"])
        self.assertFalse(m.compare_in_model(by_eps([learned(11)]), by_eps([learned(11), learned(12)]))["equal"])
        self.assertFalse(m.compare_in_model(by_eps([learned(11, mean_pnl=0.0), learned(12)]), by_eps([learned(11, mean_pnl=-0.0), learned(12)]))["equal"])
        # An in_model field on one side only, on either side.
        extra = by_eps()
        extra["0.0005"]["in_model"]["new_behavioral_result"] = {"cvar95": 999.0}
        self.assertFalse(m.compare_in_model(extra, by_eps())["equal"])
        self.assertFalse(m.compare_in_model(by_eps(), extra)["equal"])
        # Nothing to compare never passes.
        self.assertFalse(m.compare_in_model({}, {})["equal"])
        self.assertFalse(m.compare_in_model(by_eps([]), by_eps([]))["equal"])
        no_in_model = {"0.0005": {"band_w": 0.01}}
        self.assertFalse(m.compare_in_model(no_in_model, no_in_model)["equal"])
        # Missing on the reference side, and another set of half spreads.
        self.assertFalse(m.compare_in_model(by_eps(), by_eps([{k: v for k, v in learned(11).items() if k != "trades"}, learned(12)]))["equal"])
        self.assertFalse(m.compare_in_model(by_eps() | {"0.005": by_eps()["0.0005"]}, by_eps())["equal"])

    def test_run_2_of_43_compared_post_hoc(self):
        # Post-hoc regression only: run 2's in-model rows equal the
        # reference run's value by value; it changes neither its gate 6 nor
        # its outcome (REVISE).
        from pathlib import Path
        import json

        results = Path(__file__).resolve().parent.parent / "research" / "results"
        new = json.loads((results / "deep_hedging_mm.json").read_text())
        old = json.loads((results / "deep_hedging_counterpart.json").read_text())
        r = m.compare_in_model(new["by_eps"], old["by_eps"], weights=old["weights"])
        self.assertTrue(r["equal"], r)
        self.assertEqual((r["rows"], r["shared_fields"], r["metadata_fields"]), (15, 255, {"new": 15, "old": 0}))
        self.assertEqual(new["decision"]["outcome"], "revise")
