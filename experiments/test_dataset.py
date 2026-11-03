"""Dataset validator and empirical runner checks (M5-06c, #37).

Run with the conformance suite from the repository root:

    experiments/.venv/bin/python -m unittest discover -s experiments -p 'test_*.py' -v

Each corruption test copies `experiments/datasets/synthetic-v1`, breaks one
rule, refreshes the manifest hashes (except in the hash test) and checks
that the validator names that rule.
"""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path

import numpy as np

import dataset as ds
import empirical
from deep_hedging import CONFIG, ROOT

SYNTHETIC = ROOT / "experiments" / "datasets" / "synthetic-v1"


def refresh_hashes(d: Path) -> None:
    m = json.loads((d / "manifest.json").read_text())
    m["files"] = {f: ds.sha256_file(d / f) for f in ds.FILES}
    (d / "manifest.json").write_text(json.dumps(m))


def edit_csv(d: Path, name: str, fn) -> None:
    lines = (d / name).read_text().splitlines()
    (d / name).write_text("\n".join(fn(lines)) + "\n")


def edit_field(lines, row, col, value):
    cells = lines[row].split(",")
    cells[col] = value
    lines[row] = ",".join(cells)
    return lines


class Validator(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.d = Path(self.tmp.name) / "ds"
        shutil.copytree(SYNTHETIC, self.d)

    def tearDown(self):
        self.tmp.cleanup()

    def manifest(self, fn):
        m = json.loads((self.d / "manifest.json").read_text())
        fn(m)
        (self.d / "manifest.json").write_text(json.dumps(m))

    def assertViolation(self, needle):
        v = ds.validate(self.d)
        self.assertTrue(any(needle in x for x in v), f"{needle!r} not in {v[:5]}")
        with self.assertRaises(ds.DatasetInvalid):
            ds.load(self.d)

    def test_the_synthetic_dataset_is_valid_and_validation_changes_nothing(self):
        before = {f.name: f.read_bytes() for f in SYNTHETIC.iterdir()}
        self.assertEqual(ds.validate(SYNTHETIC), [])
        data = ds.load(SYNTHETIC)
        self.assertEqual({f.name: f.read_bytes() for f in SYNTHETIC.iterdir()}, before)
        self.assertEqual(data.bid.shape, (64, 31))
        self.assertEqual(data.fillable_buy.shape, (64, 30))
        self.assertTrue(data.manifest["synthetic"])

    def test_a_file_that_does_not_match_its_hash(self):
        edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 5, "711"))
        self.assertViolation("does not match the manifest")

    def test_a_missing_manifest_field(self):
        self.manifest(lambda m: m.pop("queue_model"))
        self.assertViolation("missing queue_model")

    def test_an_unknown_manifest_field(self):
        self.manifest(lambda m: m.update(extra=1))
        self.assertViolation("unknown field extra")

    def test_a_boolean_is_not_a_number(self):
        self.manifest(lambda m: m.update(lot=True))
        self.assertViolation("lot must be")

    def test_a_synthetic_dataset_without_a_seed(self):
        self.manifest(lambda m: m.update(seed=None))
        self.assertViolation("must declare its integer seed")

    def test_the_seed_is_bounded_to_u64(self):
        for seed in (2**64, -1, 10**400):
            with self.subTest(seed=seed):
                self.manifest(lambda m, seed=seed: m.update(seed=seed))
                v = [x for x in ds.validate(self.d) if "seed" in x]
                self.assertTrue(v and all("outside [0, 2**64 - 1]" in x and len(x) < 200 for x in v), v)
        self.manifest(lambda m: m.update(seed=2**64 - 1))
        self.assertFalse([x for x in ds.validate(self.d) if "seed" in x])

    def test_steps_above_the_bound_echo_a_bounded_prefix(self):
        self.manifest(lambda m: m.update(steps=10**400))
        v = [x for x in ds.validate(self.d) if "steps" in x]
        self.assertTrue(v and all("above" in x and "characters" in x and len(x) < 200 for x in v), v)

    def test_a_queue_model_without_limits(self):
        self.manifest(lambda m: m["queue_model"].update(limits=[]))
        self.assertViolation("queue_model.limits")

    def test_an_unsupported_contract(self):
        self.manifest(lambda m: m["contract"].update(style="american"))
        self.assertViolation("contract.style")

    def test_a_wrong_header(self):
        edit_csv(self.d, "execution.csv", lambda l: [l[0].replace("reject", "rejected")] + l[1:])
        refresh_hashes(self.d)
        self.assertViolation("header must be")

    def test_strict_integers(self):
        for bad in ("1.0", "True", "-1", "1e3"):
            with self.subTest(bad=bad):
                shutil.rmtree(self.d)
                shutil.copytree(SYNTHETIC, self.d)
                edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 5, bad))
                refresh_hashes(self.d)
                self.assertViolation("is not a non-negative integer")

    def test_strict_numbers(self):
        for bad in ("nan", "inf", "True", "", "1,0"):
            with self.subTest(bad=bad):
                shutil.rmtree(self.d)
                shutil.copytree(SYNTHETIC, self.d)
                if bad == "1,0":
                    edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 3, "1,0"))
                    needle = "expected 7 fields"
                else:
                    edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 3, bad))
                    needle = "is not a finite decimal number"
                refresh_hashes(self.d)
                self.assertViolation(needle)

    def test_a_crossed_book(self):
        edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 3, "2600.00"))
        refresh_hashes(self.d)
        self.assertViolation("0 < bid <= ask")

    def test_steps_must_be_step_days_apart(self):
        # A three-day gap from step 5 of path 0: still strictly increasing.
        def gap(lines):
            for i in range(1, 32):
                cells = lines[i].split(",")
                if int(cells[1]) >= 5:
                    cells[2] = str(int(cells[2]) + 2 * 86400)
                lines[i] = ",".join(cells)
            return lines

        edit_csv(self.d, "paths.csv", gap)
        refresh_hashes(self.d)
        self.assertViolation("not step_days")

    def test_the_timestamp_unit_is_declared(self):
        self.manifest(lambda m: m.update(timestamp_unit="ms"))
        self.assertViolation("timestamp_unit must be s")
        self.manifest(lambda m: m.pop("timestamp_unit"))
        self.assertViolation("missing timestamp_unit")

    def test_prices_are_on_the_tick(self):
        edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 4, "2501.195"))
        refresh_hashes(self.d)
        self.assertViolation("is not a multiple of the tick")

    def test_the_tick_check_has_no_precision_limit(self):
        # Exact on the grid, far beyond 28 significant digits: no exception,
        # and no tick violation.
        for value, on_tick in (("1e40", True), ("123456789012345678901234567890.01", True), ("1e-30", False)):
            with self.subTest(value=value):
                shutil.rmtree(self.d)
                shutil.copytree(SYNTHETIC, self.d)
                edit_csv(self.d, "paths.csv", lambda l: edit_field(edit_field(l, 1, 3, value), 1, 4, value))
                refresh_hashes(self.d)
                v = ds.validate(self.d)
                ticks = [x for x in v if "multiple of the tick" in x]
                self.assertEqual(ticks == [], on_tick, v[:5])

    def test_a_manifest_that_is_not_an_object_is_a_violation_not_an_exception(self):
        (self.d / "manifest.json").write_text("[]")
        self.assertViolation("not a JSON object")

    def test_a_lot_count_beyond_int64_is_a_violation_not_an_exception(self):
        edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 5, str(2**63)))
        refresh_hashes(self.d)
        self.assertViolation("int64 range")

    def test_a_crossed_book_hidden_by_float_rounding(self):
        def crossed(lines):
            lines = edit_field(lines, 1, 3, "100000000000000000000.01")
            return edit_field(lines, 1, 4, "100000000000000000000.00")

        edit_csv(self.d, "paths.csv", crossed)
        refresh_hashes(self.d)
        self.assertViolation("0 < bid <= ask")

    def test_prices_with_huge_exponents_or_float_underflow(self):
        for value, needle in (("1e-999999", "exponent beyond"), ("1e-350", "underflows to zero")):
            with self.subTest(value=value):
                shutil.rmtree(self.d)
                shutil.copytree(SYNTHETIC, self.d)
                edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 3, value))
                refresh_hashes(self.d)
                self.assertViolation(needle)

    def test_json_integers_beyond_the_float_range_are_violations(self):
        for field in ("tick", "lot", "fee_bps", "step_days"):
            with self.subTest(field=field):
                shutil.rmtree(self.d)
                shutil.copytree(SYNTHETIC, self.d)
                self.manifest(lambda m: m.update({field: 10**400}))
                v = ds.validate(self.d)
                self.assertTrue(any(f"{field} must be" in x and "characters" in x for x in v), v[:3])

    def test_violations_echo_a_bounded_prefix_of_the_value(self):
        edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 1, 3, "1" * 5000 + ".x"))
        refresh_hashes(self.d)
        v = [x for x in ds.validate(self.d) if "paths.csv:2" in x]
        self.assertTrue(v and all(len(x) < 200 for x in v), v)

    def test_only_the_implemented_rounding_contract_is_accepted(self):
        self.manifest(lambda m: m.update(rounding="anything-at-all"))
        self.assertViolation("rounding must be floor-lots")

    def test_a_duplicate_key(self):
        edit_csv(self.d, "execution.csv", lambda l: l[:2] + [l[1]] + l[2:])
        refresh_hashes(self.d)
        self.assertViolation("duplicate key")

    def test_a_missing_execution_row_breaks_the_join(self):
        edit_csv(self.d, "execution.csv", lambda l: l[:5] + l[6:])
        refresh_hashes(self.d)
        self.assertViolation("no 1:1 join")

    def test_timestamps_must_increase_within_a_path(self):
        edit_csv(self.d, "paths.csv", lambda l: edit_field(l, 2, 2, "1767225600"))
        refresh_hashes(self.d)
        self.assertViolation("not strictly increasing")

    def test_capacity_above_the_quoted_size(self):
        edit_csv(self.d, "execution.csv", lambda l: edit_field(l, 1, 2, "99999999"))
        refresh_hashes(self.d)
        self.assertViolation("fillable_buy")

    def test_reject_is_zero_or_one(self):
        edit_csv(self.d, "execution.csv", lambda l: edit_field(l, 1, 4, "2"))
        refresh_hashes(self.d)
        self.assertViolation("reject must be 0 or 1")

    def test_an_execution_row_at_expiry_is_not_a_decision(self):
        edit_csv(self.d, "execution.csv", lambda l: l + ["0,30,1,1,0"])
        refresh_hashes(self.d)
        self.assertViolation("is not a decision step")


class Runner(unittest.TestCase):
    def test_end_to_end_on_the_synthetic_dataset_issues_no_decision(self):
        res = empirical.run(SYNTHETIC)
        self.assertEqual(res["label"], empirical.SYNTHETIC_LABEL)
        self.assertIsNone(res["decision"])
        self.assertEqual(len(res["on_dataset"]["learned"]), len(CONFIG["train_seeds"]))
        res["_json"] = "x.json"
        self.assertIn("not empirical evidence", empirical.render(res))

    def test_the_dataset_must_match_the_trained_configuration(self):
        m = json.loads((SYNTHETIC / "manifest.json").read_text())
        self.assertEqual(empirical.compatible(m, CONFIG), [])
        for field, value in (("steps", 29), ("lot", 1e-3), ("fee_bps", 1.0), ("step_days", 0.5)):
            with self.subTest(field=field):
                self.assertNotEqual(empirical.compatible(m | {field: value}, CONFIG), [])

    def test_decisions_never_see_future_rows_or_the_execution_row_of_their_step(self):
        data = ds.load(SYNTHETIC)
        book = empirical.rebased(data, CONFIG)
        nets = empirical.frozen_nets(CONFIG)
        policy = empirical.learned_policy(nets[0][2], CONFIG)
        t0 = 12
        base = []
        empirical.simulate(policy, book, data, CONFIG, trace=base)
        # Change the execution rows from t0 on and the book after t0.
        fb, fs, rej = data.fillable_buy.copy(), data.fillable_sell.copy(), data.reject.copy()
        fb[:, t0:], fs[:, t0:], rej[:, t0:] = 0, 0, 1
        future = {k: v.copy() for k, v in book.items()}
        for k in future:
            future[k][:, t0 + 1 :] *= 1.1
        changed = ds.Dataset(**{**data.__dict__, "fillable_buy": fb, "fillable_sell": fs, "reject": rej})
        other = []
        empirical.simulate(policy, future, changed, CONFIG, trace=other)
        for t in range(t0 + 1):
            np.testing.assert_array_equal(base[t], other[t])
        self.assertFalse(np.array_equal(base[t0 + 1], other[t0 + 1]))

    def test_prices_near_the_float_maximum_rebase_finitely(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp) / "ds"
            shutil.copytree(SYNTHETIC, d)
            m = json.loads((d / "manifest.json").read_text())
            m["tick"] = 1e308
            (d / "manifest.json").write_text(json.dumps(m))

            def at_max(lines):
                for i in range(1, len(lines)):
                    lines = edit_field(edit_field(lines, i, 3, "1e308"), i, 4, "1e308")
                return lines

            edit_csv(d, "paths.csv", at_max)
            refresh_hashes(d)
            self.assertEqual(ds.validate(d), [])
            book = empirical.rebased(ds.load(d), CONFIG)
            for x in book.values():
                self.assertTrue(np.all(np.isfinite(x)) and np.all(x == CONFIG["s0"]))

    def test_a_book_that_overflows_when_rebased_is_refused_before_simulating(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp) / "ds"
            shutil.copytree(SYNTHETIC, d)
            m = json.loads((d / "manifest.json").read_text())
            m["tick"] = 1e-300
            (d / "manifest.json").write_text(json.dumps(m))

            def spread_out(lines):
                lines = edit_field(edit_field(lines, 1, 3, "1e-300"), 1, 4, "1e-300")
                return edit_field(edit_field(lines, 2, 3, "1e300"), 2, 4, "1e300")

            edit_csv(d, "paths.csv", spread_out)
            refresh_hashes(d)
            self.assertEqual(ds.validate(d), [])
            with self.assertRaises(ds.DatasetInvalid) as e:
                empirical.rebased(ds.load(d), CONFIG)
            self.assertIn("path 0", e.exception.violations[0])

    def test_a_rejected_order_fills_nothing_and_costs_nothing(self):
        data = ds.load(SYNTHETIC)
        all_rejected = ds.Dataset(**{**data.__dict__, "reject": np.ones_like(data.reject)})
        book = empirical.rebased(all_rejected, CONFIG)
        pnl, stats = empirical.simulate(empirical.delta_policy(CONFIG), book, all_rejected, CONFIG)
        self.assertEqual(float(np.sum(stats["rebalance_trades"])), 0.0)
        self.assertEqual(float(np.sum(stats["spread"])), 0.0)
        self.assertEqual(float(np.sum(stats["fee"])), 0.0)
        self.assertTrue(np.all(stats["rejected"] > 0))


if __name__ == "__main__":
    unittest.main()
