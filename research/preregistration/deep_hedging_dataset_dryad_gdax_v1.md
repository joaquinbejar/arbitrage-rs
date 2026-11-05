# Decision criterion for deep hedging on `dryad-gdax-v1` (#44)

Written and committed before any control ran on this dataset.

## Dataset

`experiments/datasets/dryad-gdax-v1`, built by `experiments/make_dryad_dataset.py` from Dryad doi:10.5061/dryad.q2bvq83rn (CC0): market `gdax`, level 1 at 00:00 UTC, 13 paths of 31 daily observations. The market was chosen from coverage, quality and its known listing, without running a control.

## Run

`experiments/empirical.py` unchanged: the frozen M5-06 controls, not retrained, with weights checked by sha256, at the trained lot and fee.

## Criterion

The statistic is the paired ΔCVaR95 = learned − band under executable execution, with its 95 % bootstrap interval, for each of the 5 seeds. Lower is better.

- **ADOPT** cannot be issued: 13 paths are too few. The synthetic pipeline check already says 64 paths have little power, and with 13 paths CVaR95 is close to the single worst path.
- **REVISE**: the lower bound of the interval is above 0 for all 5 seeds.
- **Inconclusive**: any other result.

No market, window, fee or seed is changed after seeing a result. Any other run is reported as exploratory.

## Limits

The manifest's `queue_model.limits`: capacity is an assumption, no fill is observed, and tick, lot and the 2 bp fee are the trained configuration rather than venue history. Paths may share an end date, and all come from 2018 and 2019. The result does not generalise beyond this market and period.

## Approval

Not reviewed by the two reviewers. The maintainer asked for this work directly; the reviewer status is stated in the pull request.
