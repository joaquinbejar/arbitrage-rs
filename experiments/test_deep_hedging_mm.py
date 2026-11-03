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
