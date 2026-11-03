# Preregistration: deep hedging against the market-maker-rs counterpart, run 3 (#43)

Written on 2026-11-03, before any path of run 3 exists. This file holds no
hash of itself, no code hash, no commit id and no seed; those are bound in
stages (see "Binding"), so nothing is circular. The run is
`experiments/deep_hedging_mm_run3.py`.

## Why a third run

- Run 1 aborted before writing any result.
- Run 2 gave REVISE: gate 6 failed on a metadata field the harness had
  added, and every other gate held.
- Neither run is reclassified, and neither is the reference run's ADOPT;
  their results stay as published.
- Run 3 is a new held-out evaluation after run 2: a new Heston set, from
  seeds that do not exist when this file and the code are approved, to
  whose paths no one has had prior access.

## Seeds

The author of this file and of the code does not choose the seeds. There
are two seed slots, `eval_seed` and `bootstrap_seed`, filled by the
maintainer after both approvers' GOs (see "Binding").

- **How they are drawn.** Each is drawn with a CSPRNG,
  `secrets.randbelow(2**64 - 299)` (uniform on [0, 2^64 − 300]), by the
  `fill` command; the pair is redrawn only while it breaks the fill rule.
  The authorization states the method and the number of draws. A value
  picked by hand is not a valid fill.
- **Fill rule.** Each seed is a JSON integer (not a bool, float or
  string) in [0, 2^64 − 300]. Let the bootstrap range of a seed `s` be
  [s, s + 299], the streams `paired_bootstrap` draws from it.
  - `eval_seed` is not a forbidden seed, and is in neither the reference
    run's bootstrap range nor run 3's.
  - Run 3's bootstrap range meets neither the reference run's, nor the
    dry run's (see "Dry run"), nor any forbidden seed; `eval_seed` is in
    neither of those two ranges.
  - The forbidden seeds are listed in the template (`forbidden_seeds`):
    `data_seed`, the reference `bootstrap_seed`, the train seeds, and
    every literal seed the repository uses in experiments, tests and
    datasets, and the two dummy seeds of the dry run. Runs 1 and 2 and
    the reference run drew their Heston sets from `data_seed`, so those
    are covered.
  - There are fewer than 100 train seeds, so the offsets i, 100 + i and
    200 + i never collide.
- **Streams.** `c0, c1 = np.random.SeedSequence(eval_seed).spawn(2)`. The
  paths are `heston_paths(cfg["n_test"], np.random.default_rng(c0), cfg)`
  and the market is `market(paths, np.random.default_rng(c1), cfg)`, with
  numpy's default bit generator (PCG64) at the pinned numpy version. Size,
  horizon and Heston parameters are those of run 2.
- The GBM validation and test sets keep `data_seed` and its streams, and
  the in-model rows keep `cfg["bootstrap_seed"]` and their offsets, so
  gate 6 reproduces them.
- For learned seed `i` (its index in `cfg["train_seeds"]`), each Heston
  row under each dynamics uses `default_rng(bootstrap_seed + i)` for
  `delta_cvar95_vs_band` and `default_rng(bootstrap_seed + 100 + i)` for the
  ideal execution one; the in-range subset (gate 7) uses
  `default_rng(bootstrap_seed + 200 + i)`. These are run 2's offsets, so
  both dynamics stay paired.

## Dry run

Before the code commit was fixed, the whole run was executed once, as the
approvers asked and the maintainer authorized, to find failures in the
code rather than in run 3 itself:

- on commit `05e2c9e86258e890101d61b8540871b4d0ca638b`, which differs from
  the code commit only in the dry run's seeds being forbidden, in this
  section, and in the restart window's anchor (record times instead of
  file times, so each stage line also carries its UTC time);
- in a scratch clone, through the real launch path (template, a dummy
  final manifest in a child commit, `run` with the four launch arguments
  and the thread variables at 1);
- with dummy seeds drawn by `draw_seeds` (one draw): `eval_seed`
  4659386883093802759 and `bootstrap_seed` 2043307074730982498, now
  forbidden, with their bootstrap range treated as used.

Only these were recorded: exit code 0; 191.8 s; a peak resident set of
939950080 bytes for the largest process; gate 6 equal, bit for bit, under
the pinned thread settings (it depends only on `data_seed`, so it says
nothing about any Heston set). The results file, the attempt's files, the
stage log and the captured output were deleted unread at
2026-11-03T16:11:35Z. The run 3 report states this dry run.

## What changes, and nothing else

1. **The Heston evaluation set and its bootstrap**, from the seeds above.
2. **Gate 6 compares values** (`compare_in_model`, PR #48). It reproduces
   the in-model rows on the GBM test set and compares them with the
   reference run's value by value. It requires the schema on both sides
   (band_w, in_model with delta, band and non-empty learned rows with
   unique int seeds), reports malformed input instead of raising, leaves
   out only `weights_sha256`, and requires that field on every new row,
   equal to the reference run's weight record. Its result key is
   `in_model_matches_reference`.
3. **Diagnostic A** is refitted with the corrected Python fit (PR #47): rho,
   a and b are recovered from the active constraint faces, and the fit
   refuses a slice outside the SVI domain. By the refit test of #47, rho
   becomes exactly −1.0 on days 1 to 16 (where the lower rho face is
   active) and every other parameter of every day is unchanged bit for
   bit; no a face and no Lee face is active. The run records surface A's
   sha256 and, per day, every parameter that differs bit for bit from run
   2's surface A. A stays diagnostic only; if a slice is still not
   representable, amendment 2 of the first preregistration applies as
   written.
4. **The log carries no outcome.** Until the results file exists, stdout,
   stderr and the stage log carry only stage names, elapsed seconds and
   the sha256 of each generated data array.

## What stays as it was

- the frozen weights (the 15 sha256 of the reference run);
- surface B (`deep_hedging_mm_surface_b.{csv,json}`, with their recorded
  hashes) and its recomputation check;
- the binary, the manifest, the lock file and their hashes;
- the dynamics, the primary half spread and the decision criterion;
- the agreement tolerances and their three panels;
- gates 1 to 7, the extrapolation label and the 1 % out-of-domain bound,
  as in the first preregistration with its amendments 1 and 2.

## Binding

1. **Code commit `C`** holds this file, the run 3 code and the launch
   manifest template
   (`research/preregistration/deep_hedging_mm_v2_launch.template.json`).
   - `H_P` is the SHA-256 of this file's bytes at `C` (`git cat-file blob
     C:research/preregistration/deep_hedging_mm_v2.md`): UTF-8, no byte
     order mark, LF line endings, ending in one LF.
   - The template pins: `H_P` and the first preregistration's sha256; the
     sha256 of every tracked file under `experiments/`; the run 2,
     reference run, surface B and failed run records; the 15 weight files;
     the binary's sha256; the full configurations (`CONFIG`,
     `COUNTERPART_CONFIG`, `MM_CONFIG`, so the train seeds, sizes,
     horizon, seeds and criterion thresholds); the Python, numpy and torch
     versions, the machine and the thread settings (one torch thread,
     deterministic algorithms, and `OMP_NUM_THREADS`,
     `OPENBLAS_NUM_THREADS`, `MKL_NUM_THREADS` and `VECLIB_MAXIMUM_THREADS`
     set to 1 when the process starts); the forbidden seeds. Its two seed
     fields are `null`. `T` is the sha256 of its bytes.
   - The template is in canonical form (sorted keys, two space indent,
     ASCII, one final LF); it cannot record `C`, since it is part of `C`.
2. **The GOs.** Two independent approvers each give a GO: the first
   message from that approver on the bus that says GO and names `C`, `H_P`
   and `T`. A GO withdrawn before the authorization cancels that GO; a new
   version needs a new `C`, `H_P`, `T` and new GOs.
3. **The authorization.** After both GOs, the maintainer runs `fill`,
   which changes only the two seed fields, and posts in one message the
   final manifest's exact bytes, its sha256 `L`, the draw method and count,
   and the authorization naming `C`, `H_P`, `T` and `L`. The final manifest
   is committed verbatim, as
   `research/preregistration/deep_hedging_mm_v2_launch.json`, in a child
   commit of `C` that changes nothing else. No path is generated before
   this message.
4. **The run checks**, before generating anything, against `C`, `H_P`, `T`
   and `L` given as its launch arguments (from the approvals, never read
   from the tree):
   - the tree is clean; `HEAD` has exactly one parent, `C`, and its only
     change from `C` is the added final manifest;
   - this file's bytes have sha256 `H_P` and contain no CR byte or byte
     order mark;
   - the template's bytes have sha256 `T` and the final manifest's bytes
     sha256 `L`; both parse as canonical JSON, with no duplicate key and
     no NaN or infinity;
   - the template's seeds are exactly two nulls; every other field of the
     template equals, with strict types, what the run finds in the tree
     and the environment, and pins one thread everywhere and deterministic
     algorithms;
   - the final manifest has exactly the template's fields and equals it,
     with strict types (a bool is not an int, an int not a float), in
     every field except the seeds; the seeds satisfy the fill rule.
   Any difference stops the run before step 2 of "Order of the run".

## Order of the run

One unattended process with the pinned settings. In this order:

1. Checks (see "Binding"). The results file must not exist and no attempt
   may be on record (a restart: see "Failures"). The run creates its
   attempt directory (`research/results/deep_hedging_mm_run3/attempt-N/`)
   and publishes the execution manifest there: `C`, `H_P`, `T`, `L`, the
   seeds, `HEAD`, the host, the environment, the binary's sha256 and the
   start time. The attempt starts when that file exists.
2. Data: the GBM validation and test sets (`data_seed`), then the new
   Heston set (`eval_seed`); the stage log records each array's sha256.
3. Conformance (gate 1); surface B recomputed and compared with the
   committed files (gate 2); surface A refitted.
4. Coverage of the new set on surface B (gate 3 and the extrapolation
   label); the three agreement panels on surface B (gate 5); diagnostic A.
5. For each half spread: the band on the validation set, the in-model rows
   on the test set, the Heston rows under each dynamics, each with its
   paired bootstrap.
6. Gate 6 (`compare_in_model`) against the reference run.
7. The in-range subset at the primary half spread (gate 7).
8. The criterion, the gates and the outcome. The results are written to a
   temporary file in the attempt's directory and linked to
   `research/results/deep_hedging_mm_run3.json` in one step, which fails if
   that path exists: the results appear complete or not at all, and are
   never replaced. Then the decision is printed.

Every file of an attempt is created new and never replaced.

## No look before the run

- No one, the maintainer included, generates paths from the filled seeds or
  computes anything from them before the run; tests use other seeds.
- The run starts within 1 hour of the authorization message. A run not
  started in that window, or abandoned after it starts, is a recorded
  final failure, never "no run".
- During the run, no one reads intermediate output, and nothing is tuned,
  stopped or restarted on it. The log carries no outcome, but that is a
  safeguard; the commitment is procedural.
- Coverage is computed inside the run, from the new paths and surface B
  only, and reported. The 1 % out-of-domain bound is a gate. Nothing is
  selected on coverage, and no seed is screened.

## Decision

ADOPT requires every one of these:

- **The criterion.** in_model holds on the GBM test set. There is no veto
  on the new Heston set under either dynamics: no seed has the lower bound
  of the paired 95 % CI of CVaR95(learned) − CVaR95(band) above 0.
- **Gates 1 to 7.**
  - Gate 6 compares values.
  - Gate 5 needs 0 violations in all three panels.
  - Gate 7 needs no veto on the in-range subset of the new set.

Otherwise the outcome is REVISE, or REJECT under the criterion's own rule.

## Failures

- **Before the authorization**, a check that fails is fixed by a new
  version (see "Amendments").
- **After the authorization, every failure is final**, apart from the one
  restart below, and the seeds are never drawn again. A check of step 1
  that fails generates no path; the run records it
  (`research/results/deep_hedging_mm_run3_preflight_failure.json`) and run
  3 ends there.
- **Infrastructure interruption.** After the execution manifest exists and
  before the results exist, the process ends for one of these causes,
  shown by the system and not by anyone's account:
  - a `MemoryError`, or an `OSError` with errno `ENOSPC`, `EIO` or
    `EDQUOT`, in the attempt's exception record. The run writes that
    record for any exception after the execution manifest exists, the
    stage log's creation included; when it cannot (a full disk), the
    traceback printed on stderr is copied verbatim into the failure
    record instead;
  - a kill by the operating system's out-of-memory handler, or a host
    restart or power loss, shown by a system record: a verbatim excerpt of
    the out-of-memory log, the system journal or the boot record, copied
    into the attempt's failure record, which both approvers read before
    the restart.
  A stop by an operator is not one. The run may then be restarted once,
  with `--restart`, from the same commit with the same approvals, within
  1 hour of the last time attempt 1 wrote in its own records (the
  execution manifest's start, the stage lines, the exception record; file
  times do not count). The run refuses a later restart, and a record time
  in the future:
  - attempt 1 must be complete on record (its execution manifest and stage
    log, and its exception record or failure record), and is kept
    verbatim;
  - attempt 2's execution manifest must equal attempt 1's apart from the
    attempt number and the start time, so the commit, approvals, seeds,
    host, environment and binary are the same;
  - the run is deterministic under the pinned settings: a restart computes
    the same arrays and the same outputs, so it has no option value. Every
    data array hash attempt 1 logged must be reproduced, or the restart is
    a final failure;
  - a second interruption is final.
- **Anything else is final.** Any other exception or exit after the
  attempt starts is a final failure of run 3: a fit, domain, counterpart,
  numerical or coverage failure, or anything after the results exist. The
  attempt's exception record and stage log are kept. Run 1's cause (a
  slice outside the binary's domain) would be final under this rule.

## Amendments

- Before the authorization, a change to this file, the code or the
  template makes a new version (new `C`, `H_P`, `T`) and a new review; the
  version lists the change and its reason here.
- After the authorization, nothing changes, and no withdrawal of a GO
  changes anything. A later problem ends run 3 under "Failures"; it never
  resets the protocol to draw other seeds.

## Final

- Run 3 runs once, apart from the single restart above.
- A failed or non-ADOPT run 3 is final. A run 4 needs a new
  preregistration and a reason other than the result.
- The report calls run 3 a new held-out evaluation after run 2, with no
  prior access to its paths, and keeps runs 1 and 2 with their outcomes.
