"""Run 3 of #43: the binding, the seeds and the file policy, with negative
cases. No test generates data from a seed run 3 may use."""

import errno
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

import numpy as np

import deep_hedging_mm_run3 as r3
from deep_hedging import CONFIG

GOOD = {"eval_seed": 10**12, "bootstrap_seed": 10**15}
FORBIDDEN = r3.forbidden_seeds(CONFIG)


def pins(**over):
    p = {
        "schema": r3.SCHEMA,
        "preregistration": {"path": r3.PREREGISTRATION_REL, "sha256": None},
        "files": {"experiments/x.py": "0" * 64},
        "config": {"n": 1, "flag": True, "x": 0.5},
        "forbidden_seeds": FORBIDDEN,
    }
    p.update(over)
    return p


class Repo:
    """A git repository laid out like this one: commit C with the
    preregistration and the template, then a child adding the final
    manifest."""

    def __init__(self, tmp: Path, prereg: bytes = b"# protocol\n", seeds=None, template_over=None, final_over=None):
        self.root = tmp
        self.git("init", "-q")
        self.prereg = prereg
        self.write(r3.PREREGISTRATION_REL, prereg)
        self.pins = pins(preregistration={"path": r3.PREREGISTRATION_REL, "sha256": r3.sha256_bytes(prereg)})
        template = self.pins | {"seeds": {k: None for k in r3.SEED_SLOTS}} | (template_over or {})
        self.template_raw = r3.canonical_bytes(template)
        self.write(r3.TEMPLATE_REL, self.template_raw)
        self.commit("C")
        self.c = self.git("rev-parse", "HEAD").strip()
        final = template | {"seeds": seeds or GOOD} | (final_over or {})
        self.final_raw = r3.canonical_bytes(final)
        self.write(r3.MANIFEST_REL, self.final_raw)
        self.commit("launch")

    def git(self, *args):
        cmd = ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", *args]
        return subprocess.run(cmd, cwd=self.root, check=True, capture_output=True, text=True).stdout

    def write(self, rel, raw: bytes):
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(raw)

    def commit(self, msg):
        self.git("add", "-A")
        self.git("commit", "-q", "-m", msg)

    def args(self, **over):
        a = {
            "code_commit": self.c,
            "prereg_sha": r3.sha256_bytes(self.prereg),
            "template_sha": r3.sha256_bytes(self.template_raw),
            "manifest_sha": r3.sha256_bytes(self.final_raw),
        }
        a.update(over)
        return a

    def verify(self, restart=False, facts=None, **over):
        return r3.verify_launch(self.root, **self.args(**over), restart=restart, facts_fn=lambda root: facts if facts is not None else self.pins)


class Binding(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def repo(self, **kw):
        d = self.dir / f"r{len(list(self.dir.iterdir()))}"
        d.mkdir()
        return Repo(d, **kw)

    def test_the_approved_tree_passes(self):
        final = self.repo().verify()
        self.assertEqual(final["seeds"], GOOD)

    def test_the_approved_values_come_from_the_caller(self):
        repo = self.repo()
        other = self.repo(prereg=b"# another protocol\n", seeds={"eval_seed": 10**13, "bootstrap_seed": 10**16})
        self.assertNotEqual(other.c, repo.c)
        cases = {
            "another commit as C": {"code_commit": other.c},
            "another preregistration hash": {"prereg_sha": r3.sha256_bytes(other.prereg)},
            "another template hash": {"template_sha": r3.sha256_bytes(other.template_raw)},
            "another manifest hash": {"manifest_sha": r3.sha256_bytes(other.final_raw)},
            "upper case": {"code_commit": repo.c.upper()},
            "short": {"template_sha": "ab"},
        }
        for name, over in cases.items():
            with self.subTest(name), self.assertRaises(r3.LaunchError):
                repo.verify(**over)

    def test_a_self_consistent_substitute_is_refused(self):
        # A tree with another template and a manifest consistent with it:
        # every internal check holds, but T and L are not the approved ones.
        approved = self.repo()
        substitute = self.repo(template_over={"config": {"n": 2, "flag": True, "x": 0.5}})
        with self.assertRaises(r3.LaunchError):
            substitute.verify(
                facts=substitute.pins | {"config": {"n": 2, "flag": True, "x": 0.5}},
                code_commit=substitute.c,
                template_sha=approved.args()["template_sha"],
                manifest_sha=approved.args()["manifest_sha"],
            )

    def test_another_parent_or_more_changes_are_refused(self):
        repo = self.repo()
        repo.write("experiments/extra.py", b"x = 1\n")
        repo.commit("more")
        with self.assertRaises(r3.LaunchError):
            repo.verify()
        repo = self.repo()
        repo.git("reset", "-q", "--soft", "HEAD~1")
        repo.write("experiments/extra.py", b"x = 1\n")
        repo.commit("launch with more")
        with self.assertRaises(r3.LaunchError):
            repo.verify(manifest_sha=r3.sha256_bytes(repo.final_raw))

    def test_a_dirty_tree_is_refused_except_attempt_1_on_a_restart(self):
        repo = self.repo()
        repo.write("notes.txt", b"x\n")
        with self.assertRaises(r3.LaunchError):
            repo.verify()
        (repo.root / "notes.txt").unlink()
        repo.write(f"{r3.ATTEMPTS_REL}/attempt-1/stage.log", b"{}\n")
        with self.assertRaises(r3.LaunchError):
            repo.verify()
        self.assertEqual(repo.verify(restart=True)["seeds"], GOOD)
        repo.write(f"{r3.ATTEMPTS_REL}/attempt-2/stage.log", b"{}\n")
        with self.assertRaises(r3.LaunchError):
            repo.verify(restart=True)

    def test_the_preregistration_bytes(self):
        for raw in (b"# protocol\r\n", b"\xef\xbb\xbf# protocol\n"):
            with self.subTest(raw), self.assertRaises(r3.LaunchError):
                self.repo(prereg=raw).verify()

    def test_the_manifest_must_equal_the_template_outside_the_seeds(self):
        cases = {
            "a changed pin": {"files": {"experiments/x.py": "1" * 64}},
            "bool for int": {"config": {"n": True, "flag": True, "x": 0.5}},
            "int for bool": {"config": {"n": 1, "flag": 1, "x": 0.5}},
            "int for float": {"config": {"n": 1, "flag": True, "x": 1}},
            "an extra field": {"note": "x"},
            "an extra seed": {"seeds": GOOD | {"third": 1}},
        }
        for name, over in cases.items():
            with self.subTest(name), self.assertRaises(r3.LaunchError):
                self.repo(final_over=over).verify()

    def test_the_template_must_match_the_tree_and_hold_null_seeds(self):
        repo = self.repo()
        with self.assertRaises(r3.LaunchError):
            repo.verify(facts=repo.pins | {"files": {"experiments/x.py": "2" * 64}})
        for over in ({"seeds": {"eval_seed": 0, "bootstrap_seed": None}}, {"seeds": {"eval_seed": None}}):
            with self.subTest(over), self.assertRaises(r3.LaunchError):
                self.repo(template_over=over).verify()

    def test_seeds_that_break_the_fill_rule_are_refused(self):
        for seeds in ({"eval_seed": CONFIG["data_seed"], "bootstrap_seed": 10**15}, {"eval_seed": True, "bootstrap_seed": 10**15}):
            with self.subTest(seeds), self.assertRaises(r3.LaunchError):
                self.repo(seeds=seeds).verify()


class CanonicalJson(unittest.TestCase):
    def test_round_trip(self):
        obj = {"b": [1, 2.5, None, True], "a": {"x": "y"}}
        self.assertEqual(r3.parse_canonical(r3.canonical_bytes(obj), "t"), obj)

    def test_refused(self):
        good = r3.canonical_bytes({"a": 1})
        cases = {
            "duplicate key": b'{\n  "a": 1,\n  "a": 2\n}\n',
            "NaN": b'{\n  "a": NaN\n}\n',
            "Infinity": b'{\n  "a": Infinity\n}\n',
            "not canonical": b'{"a": 1}\n',
            "no final LF": good[:-1],
            "CR": good.replace(b"\n", b"\r\n"),
            "BOM": b"\xef\xbb\xbf" + good,
            "not UTF-8": b'{\n  "a": "\xff"\n}\n',
            "an array": b"[\n  1\n]\n",
            "not JSON": b"{\n",
        }
        for name, raw in cases.items():
            with self.subTest(name), self.assertRaises(r3.LaunchError):
                r3.parse_canonical(raw, "t")


class FillRule(unittest.TestCase):
    def test_a_valid_pair(self):
        self.assertEqual(r3.fill_rule(GOOD, CONFIG, FORBIDDEN), [])
        self.assertEqual(r3.fill_rule({"eval_seed": 0, "bootstrap_seed": r3.MAX_SEED - 1}, CONFIG, FORBIDDEN), [])

    def test_forbidden_seeds_cover_the_configuration_and_the_literals(self):
        for s in (CONFIG["data_seed"], CONFIG["bootstrap_seed"], *CONFIG["train_seeds"], *r3.LITERAL_SEEDS):
            self.assertIn(s, FORBIDDEN)

    def test_broken(self):
        ref = CONFIG["bootstrap_seed"]
        cases = {
            "bool": {"eval_seed": True, "bootstrap_seed": 10**15},
            "float": {"eval_seed": 1e12, "bootstrap_seed": 10**15},
            "string": {"eval_seed": "1", "bootstrap_seed": 10**15},
            "null": {"eval_seed": None, "bootstrap_seed": 10**15},
            "negative": {"eval_seed": -1, "bootstrap_seed": 10**15},
            "too large": {"eval_seed": 10**12, "bootstrap_seed": r3.MAX_SEED + 1},
            "2^64": {"eval_seed": 2**64, "bootstrap_seed": 10**15},
            "data_seed": {"eval_seed": CONFIG["data_seed"], "bootstrap_seed": 10**15},
            "a train seed": {"eval_seed": CONFIG["train_seeds"][0], "bootstrap_seed": 10**15},
            "a literal seed": {"eval_seed": 20261103, "bootstrap_seed": 10**15},
            "in the reference bootstrap range": {"eval_seed": ref + 299, "bootstrap_seed": 10**15},
            "in its own bootstrap range": {"eval_seed": 10**15 + 5, "bootstrap_seed": 10**15},
            "bootstrap meets the reference range": {"eval_seed": 10**12, "bootstrap_seed": ref + 299},
            "bootstrap below and into the reference range": {"eval_seed": 10**12, "bootstrap_seed": 0},
            "bootstrap range holds a forbidden seed": {"eval_seed": 10**12, "bootstrap_seed": CONFIG["data_seed"] - 10},
            "a missing slot": {"eval_seed": 10**12},
            "an extra slot": GOOD | {"x": 1},
            "not an object": [10**12, 10**15],
        }
        for name, seeds in cases.items():
            with self.subTest(name):
                self.assertTrue(r3.fill_rule(seeds, CONFIG, FORBIDDEN))
        self.assertTrue(r3.fill_rule(GOOD, CONFIG | {"train_seeds": list(range(1000, 1100))}, FORBIDDEN))

    def test_draws_redraw_only_a_broken_pair(self):
        values = iter([CONFIG["data_seed"], 10**15, 10**12, 10**15])
        seeds, draws = r3.draw_seeds(CONFIG, FORBIDDEN, randbelow=lambda n: next(values))
        self.assertEqual((seeds, draws), (GOOD, 2))
        seen = []
        r3.draw_seeds(CONFIG, FORBIDDEN, randbelow=lambda n: seen.append(n) or 10**12 + len(seen) * 10**14)
        self.assertEqual(set(seen), {r3.MAX_SEED + 1})


class Attempts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.attempts = self.dir / "attempts"
        self.out = self.dir / "run3.json"
        self.execution = {"launch": {"code_commit": "c" * 40}, "seeds": GOOD, "host": "h", "started_at": "t0"}

    def tearDown(self):
        self.tmp.cleanup()

    def first(self):
        here, previous = r3.open_attempt(self.attempts, self.out, self.execution, restart=False)
        log = r3.StageLog(here / "stage.log", previous, 0.0)
        log.stage("data", {"x": np.arange(4.0)})
        log.f.close()
        return here

    def test_a_first_attempt(self):
        here = self.first()
        m = json.loads((here / "execution_manifest.json").read_text())
        self.assertEqual(m["attempt"], 1)
        with self.assertRaises(r3.AttemptError):
            r3.open_attempt(self.attempts, self.out, self.execution, restart=False)

    def test_existing_results_stop_everything(self):
        self.out.write_text("{}")
        for restart in (False, True):
            with self.subTest(restart), self.assertRaises(r3.AttemptError):
                r3.open_attempt(self.attempts, self.out, self.execution, restart=restart)

    def test_a_restart_needs_an_infrastructure_cause(self):
        here = self.first()
        later = self.execution | {"started_at": "t1"}
        with self.assertRaises(r3.AttemptError):
            r3.open_attempt(self.attempts, self.out, later, restart=True)
        for record in ({"type": "KeyboardInterrupt", "errno": None}, {"type": "SystemExit", "errno": None}, {"type": "OSError", "errno": errno.ENOENT}):
            p = here / "exception.json"
            p.write_text(json.dumps(record))
            with self.subTest(record), self.assertRaises(r3.AttemptError):
                r3.open_attempt(self.attempts, self.out, later, restart=True)
            p.unlink()
        for record in ({"cause": "operator"}, {"cause": "oom-kill"}, {"cause": "oom-kill", "system_record": ""}):
            p = here / "failure.json"
            p.write_text(json.dumps(record))
            with self.subTest(record), self.assertRaises(r3.AttemptError):
                r3.open_attempt(self.attempts, self.out, later, restart=True)
            p.unlink()

    def test_a_valid_restart_reproduces_the_data(self):
        here = self.first()
        (here / "exception.json").write_text(json.dumps({"type": "OSError", "errno": errno.ENOSPC}))
        with self.assertRaises(r3.AttemptError):
            r3.open_attempt(self.attempts, self.out, self.execution | {"host": "other"}, restart=True)
        second, previous = r3.open_attempt(self.attempts, self.out, self.execution | {"started_at": "t1"}, restart=True)
        self.assertEqual(second.name, "attempt-2")
        log = r3.StageLog(second / "stage.log", previous, 0.0)
        with self.assertRaises(r3.AttemptError):
            log.stage("data", {"x": np.arange(5.0)})
        log.stage("data", {"x": np.arange(4.0)})
        log.f.close()
        with self.assertRaises(r3.AttemptError):
            r3.open_attempt(self.attempts, self.out, self.execution, restart=True)

    def test_a_system_cause_with_its_record(self):
        here = self.first()
        (here / "failure.json").write_text(json.dumps({"cause": "host-restart", "system_record": "boot record"}))
        second, _ = r3.open_attempt(self.attempts, self.out, self.execution, restart=True)
        self.assertEqual(second.name, "attempt-2")

    def test_classification(self):
        self.assertEqual(r3.classify_exception({"type": "MemoryError"}), "MemoryError")
        for e, name in ((errno.ENOSPC, "ENOSPC"), (errno.EIO, "EIO"), (errno.EDQUOT, "EDQUOT")):
            self.assertEqual(r3.classify_exception({"type": "OSError", "errno": e}), name)
        for rec in ({"type": "ValueError"}, {"type": "OSError", "errno": errno.EEXIST}, {"type": "KeyboardInterrupt"}):
            self.assertIsNone(r3.classify_exception(rec))

    def test_results_are_published_once_and_whole(self):
        r3.publish_exclusive(self.out, b"one\n")
        with self.assertRaises(FileExistsError):
            r3.publish_exclusive(self.out, b"two\n")
        self.assertEqual(self.out.read_bytes(), b"one\n")
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()), ["run3.json"])
        with self.assertRaises(FileExistsError):
            r3.write_exclusive(self.out, b"three\n")


class Streams(unittest.TestCase):
    def test_the_heston_set_is_the_spawned_streams(self):
        cfg = CONFIG | {"n_test": 64}
        got = r3.heston_set(101, cfg)
        c0, c1 = np.random.SeedSequence(101).spawn(2)
        from deep_hedging import heston_paths, market

        want = market(heston_paths(64, np.random.default_rng(c0), cfg), np.random.default_rng(c1), cfg)
        self.assertEqual(set(got), set(want))
        for k in want:
            np.testing.assert_array_equal(got[k], want[k])
        self.assertFalse(np.array_equal(got["s"], r3.heston_set(202, cfg)["s"]))

    def test_the_bootstrap_seed_replaces_only_the_bootstrap(self):
        from deep_hedging import paired_bootstrap

        h = r3.with_bootstrap_seed(CONFIG, 202)
        self.assertEqual({k for k in CONFIG if CONFIG[k] != h[k]}, {"bootstrap_seed"})
        a, b = np.random.default_rng(101).normal(size=(2, 200))
        small = {"bootstrap": 50}
        self.assertEqual(paired_bootstrap(a, b, 0.95, CONFIG | small, seed_offset=3), paired_bootstrap(a, b, 0.95, {"bootstrap_seed": CONFIG["bootstrap_seed"]} | small, seed_offset=3))
        self.assertNotEqual(paired_bootstrap(a, b, 0.95, h | small, seed_offset=3), paired_bootstrap(a, b, 0.95, CONFIG | small, seed_offset=3))

    def test_array_hashes_see_dtype_shape_and_values(self):
        x = np.arange(6.0)
        self.assertEqual(r3.array_sha256(x), r3.array_sha256(x.copy()))
        nudged = x.copy()
        nudged[5] += 1e-12
        for y in (x.reshape(2, 3), x.astype(np.float32), nudged):
            self.assertNotEqual(r3.array_sha256(x), r3.array_sha256(y))


class Template(unittest.TestCase):
    def test_the_committed_template_is_the_tree(self):
        if not r3.TEMPLATE.exists():
            self.skipTest("no template yet")
        template = r3.parse_canonical(r3.TEMPLATE.read_bytes(), "template")
        facts = r3.launch_facts()
        # The environment is the run host's, checked at launch; CI runs elsewhere.
        strip = lambda d: {k: v for k, v in d.items() if k not in ("environment", "seeds")}  # noqa: E731
        self.assertEqual(r3._strict_diff(strip(facts), strip(template), "template"), [])
        self.assertEqual(template["seeds"], {"eval_seed": None, "bootstrap_seed": None})
        self.assertEqual(template["environment"]["torch_threads"], 1)
        self.assertIs(template["environment"]["deterministic_algorithms"], True)

    def test_facts_cover_the_run(self):
        facts = r3.launch_facts()
        self.assertIn("experiments/deep_hedging_mm_run3.py", facts["files"])
        self.assertIn("experiments/deep_hedging_mm.py", facts["files"])
        self.assertEqual(len(facts["weights"]), 15)
        self.assertEqual(facts["preregistration"]["path"], r3.PREREGISTRATION_REL)
        self.assertEqual(facts["forbidden_seeds"], FORBIDDEN)

    def test_the_environment_is_plain_json(self):
        env = r3.environment()
        self.assertEqual({k: type(v) for k, v in env.items()}, {
            "python": str, "numpy": str, "torch": str, "machine": str, "torch_threads": int, "deterministic_algorithms": bool,
        })

    def test_scrubbed_records_hold_no_local_path(self):
        text = r3._scrub(f"{r3.ROOT}/x and {Path.home()}/y")
        self.assertNotIn(str(Path.home()), text)


if __name__ == "__main__":
    unittest.main()
