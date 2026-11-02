# Exact arithmetic rule

Every amount, price, quantity and rate in arbitrage-rs is an exact `Decimal`.
Arithmetic is exact or it fails; nothing is rounded unless a caller asks for
it with an explicit `Rounding` mode. On top of that, this rule applies to all
code and to every new PR (adopted from margin-rs):

> Any aggregation or product of more than two terms goes through the
> wide-integer helpers in `src/wide.rs`, with **one** final conversion, or
> **one** final rounding. Chaining `add`, `sub`, `mul`, `fee_on`,
> `checked_add` or `net_of` over `Decimal` values is not allowed when an
> intermediate could be unrepresentable while the result is not.

| Helper | Use |
|---|---|
| `wide::sum(values)` | exact sum of any number of terms, independent of order |
| `wide::product(values)` | exact product of any number of factors, unconverted |
| `wide::sum_products(terms)` | exact signed sum of products, unconverted |
| `wide::sum_products_exact(terms)` | the same, converted once without rounding |
| `wide::sum_products_round(terms, dp, mode)` | the same, rounded once |
| `FeeRate::fee_on_factors(factors, ccy)` | `|product of factors| * bps / 10_000` as one product |
| `Cost::total(costs, ccy)` | exact sum of costs in one currency |

The wide integers have 768 bits: a product of three `Decimal` factors aligned
to a common scale fits with room to spare, so a conversion fails only when the
figure itself does not fit in a `Decimal`.

A single `add`, `sub` or `mul` of two values is fine: it is exact or it fails.
The same holds for an update of stored state by one value, because the stored
state has to be representable anyway.

## Audit (exact aggregation PR, #21)

Each row is a site that chained operations before. "Uniform" marks a site
whose terms cannot cancel (they only grow), changed to keep the rule uniform;
no input makes the old code fail there while the result fits.

| Site | Before | After | Test |
|---|---|---|---|
| `FeeRate::fee_on` | `mul(|n|, bps)`, then `div_pow10(4)` | one product `[|n|, bps, 1e-4]` | `cost::tests::fee_on_a_notional_near_max_is_exact` (MAX at 4 bps) |
| `opportunity::evaluate`, gross | `p_bid * q - p_ask * q` from two notionals | one `sum_products_exact` | `gross_and_fees_are_exact_when_notionals_pass_max` |
| `opportunity::evaluate`, fees and carry | `fee_on(p * q)` | `fee_on_factors([p, q])` | same test |
| `opportunity::evaluate`, cost total | running `checked_add` over fees and carry | `Cost::total` | `opportunity::cost_total_is_one_exact_sum` (fees BIG and BIG + 1, carry -BIG) |
| `basis::evaluate_spot_perp`, gross | `(perp - spot - exit) * q` chained | one `sum_products_exact` | `basis::gross_is_exact_when_notionals_pass_max` |
| `basis::evaluate_spot_perp`, fees and carry | `fee_on(p * q)` | `fee_on_factors([p, q])` | same test |
| `basis::evaluate_spot_perp`, totals | running `checked_add` over fees, carry and funding | `Cost::total` for the funding total and for the total | `basis::cost_total_is_one_exact_sum` |
| `TwoLegPlan::net_cash` | running signed `checked_add` / `checked_sub` | one `wide::sum` | `legs::net_cash_is_one_exact_sum` (buy BIG, fee BIG, sell BIG) |
| `TwoLegPlan`, filled notional | stored `+ p * q` | one `sum_products_exact` | uniform (terms only grow) |
| `LegState::executed`, `remaining`, `TwoLegPlan::residual` | running `add` over attempts, then `sub` | one `wide::sum` each | uniform; covered by the legs fixtures |
| `venues::requirement`, fee | `fee_on(p * q)` | `fee_on_factors([p, q])` | `venues::margin_requirement_is_exact_when_the_notional_passes_max` |
| `venues::requirement`, margin sell | `(p * q) * rate + fee` | one `sum_products_exact` | same test (a notional beyond MAX at 50 % margin) |
| `venues::requirement`, buy | `p * q + fee` | one `sum_products_exact` | uniform (the fee is clamped at zero) |
| `hedge::spread_cost` | `mid = (bid + ask) * 0.5`, then `(ask - mid) * q` | `0.5 * ask * q - 0.5 * bid * q` as one sum (equal for both sides) | `hedge::spread_cost_and_fee_are_exact_near_max` |
| `hedge::plan_band_hedge`, fee | `fee_on(p * q)` | `fee_on_factors([p, q])` | same test |
| `hedge_action`, fee | `fee_on(p * q)` | `fee_on_factors([p, q])` | `hedge_action::fee_is_exact_when_the_notional_passes_max` |

Already under the rule when they were written:

- `parity` (#20): gross, fees, fee total and net;
- `package` (#19): executed, remaining, excess, fees, net cash and filled
  notional;
- `funding`: the cashflow `position * mark * rate` is one wide product,
  rounded once.

Two-term operations kept as they are, because each side is representable:

- `opportunity` and `basis`: `net = gross - total`, and the reported
  `entry_basis = perp - spot`;
- `basis`: the implied exit price `spot + exit_basis`, which must be a valid
  price on its own;
- `legs` and `package`: per-attempt `cum_qty`, fees per leg, and unwind
  differences;
- `venues`: budget reservations and releases;
- `hedge` and `hedge_action`: `exposure +/- q`, and the fee plus the spread
  cost.
