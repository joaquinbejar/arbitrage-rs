# Conclusion on `dryad-gdax-v1` (#44): inconclusive

Under the criterion committed before the run (`research/preregistration/deep_hedging_dataset_dryad_gdax_v1.md`), the outcome is **inconclusive**. REVISE needs the lower bound of the executable ΔCVaR95 interval above 0 for all 5 seeds; seed 11 gives 1.432 [-0.143, 1.432].

Observations, not findings:

- The other four seeds have intervals above 0, and every learned control has a higher CVaR95 than the band (10.56 to 13.71 against 9.28). There is no sign here that the learned controls improve on the band.
- With 13 paths, CVaR95 is close to the worst path, and the point estimate sits at or near an edge of several intervals. These intervals are not reliable at this sample size.
- Every control has CVaR95 below its ideal execution figure. Capacity limits lowered the tail loss on these paths; that is a property of these paths and the assumed capacity, not a benefit of limits.

Limits: the manifest's `queue_model.limits` apply. No fill is observed, the 2 bp fee is an assumption, the delta and band controls use sigma = 0.2 and the premium is the M5-06 Black-Scholes price on a market with different volatility, and the data covers one market in 2018 and 2019. This says nothing about other markets or periods.
