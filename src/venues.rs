//! Per-venue views, budgets and admission of two-leg plans.
//!
//! Each leg trades on one venue. Before a plan starts, both legs are
//! admitted against **their own** venue:
//!
//! - the venue's view of the leg's instrument must be fresh (observed no more
//!   than `max_staleness_ms` before the decision, and not after it) and must
//!   quote the side the leg trades, at a price within the leg's limit and
//!   with enough size;
//! - the leg's instrument must be live and its effective deadline (own
//!   deadline or instrument expiry) after the decision;
//! - the capital the leg needs is **reserved** in that venue's
//!   [`VenueBudget`]: cash for a buy (notional at the limit price, an upper
//!   bound, plus the fee), inventory plus the fee for a sell from
//!   inventory, or margin cash for a short sale (`notional * rate` plus the
//!   fee). A sell limit is a lower bound on its price, so a sell's notional
//!   uses the higher of its limit and the observed executable bid.
//!
//! The leg's venue is [`LegSpec::venue`], which every leg must name; a view
//! or budget of another venue never applies to it. Inventory is keyed by
//! instrument: the same asset must be recorded under one instrument per
//! venue. Reservations are static amounts: the caller releases them
//! ([`VenueBudget::release`]) when the leg ends or is resized. A plan with
//! budget gating
//! ([`TwoLegPlan::with_budget_gating`](crate::legs::TwoLegPlan::with_budget_gating))
//! takes the admission's reservations over, replaces them as it readmits
//! resubmissions, and releases the ones it holds
//! ([`TwoLegPlan::release_reservations`](crate::legs::TwoLegPlan::release_reservations));
//! the caller then never releases the admission's reservations itself.
//!
//! Capital is never borrowed across venues: a leg only reserves from the
//! budget of its own venue, a venue without a budget admits nothing, and
//! there is no transfer or netting between venues. Two legs on the same
//! venue share its budget and their requirements add up. Admission is all or
//! nothing: if either leg is rejected, nothing is reserved.

use crate::cost::FeeRate;
use crate::error::{ArbitrageError, Result};
use crate::exact;
use crate::instrument::{Action, Dir, Instrument, InstrumentId, Timestamp, TopOfBook};
use crate::legs::{LegIndex, LegSpec};
use crate::money::{Currency, Money};
use crate::units::{Price, Quantity};
use crate::wide;
use core::fmt;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// A venue identifier: 1 to 64 printable ASCII characters, no whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VenueId(String);

impl VenueId {
    /// Builds a venue identifier.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] if `id` is empty, longer than 64
    /// characters or not printable ASCII.
    pub fn new(id: &str) -> Result<Self> {
        let valid = !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_graphic());
        if valid {
            Ok(Self(id.to_owned()))
        } else {
            Err(ArbitrageError::InvalidInput {
                reason: "venue id must be 1 to 64 printable ASCII characters",
            })
        }
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VenueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One venue's view of one instrument, stamped with when it was observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VenueView {
    /// The venue.
    pub venue: VenueId,
    /// The instrument quoted.
    pub instrument: Instrument,
    /// When the venue published this view.
    pub observed_at: Timestamp,
    /// Its top of book.
    pub book: TopOfBook,
}

impl VenueView {
    /// Age of the view at `as_of`, in milliseconds.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::LookAhead`] if the view is observed after `as_of`.
    pub fn age_ms(&self, as_of: Timestamp) -> Result<i64> {
        if self.observed_at > as_of {
            return Err(ArbitrageError::LookAhead {
                known_at: self.observed_at,
                as_of,
            });
        }
        Ok(as_of.millis().saturating_sub(self.observed_at.millis()))
    }
}

/// How a sell leg is funded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SellFunding {
    /// Sell inventory held on the venue: reserves the quantity.
    Inventory,
    /// Short sale on margin: reserves `notional * rate` in cash.
    Margin {
        /// Initial margin as a fraction of notional, in `(0, 1]`.
        rate: Decimal,
    },
}

/// Cash and inventory one account holds on one venue, with what is reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueBudget {
    venue: VenueId,
    cash: Money,
    reserved_cash: Money,
    inventory: BTreeMap<InstrumentId, Decimal>,
    reserved_inventory: BTreeMap<InstrumentId, Decimal>,
}

impl VenueBudget {
    /// A budget of `cash` on `venue`, with no inventory.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidValue`] if `cash` is negative.
    pub fn new(venue: VenueId, cash: Money) -> Result<Self> {
        if cash.is_negative() {
            return Err(ArbitrageError::InvalidValue {
                field: "venue cash",
                value: cash.amount(),
                reason: "must not be negative",
            });
        }
        Ok(Self {
            venue,
            reserved_cash: Money::zero(cash.currency()),
            cash,
            inventory: BTreeMap::new(),
            reserved_inventory: BTreeMap::new(),
        })
    }

    /// Adds `quantity` of `instrument` to the inventory held on the venue.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn with_inventory(mut self, instrument: InstrumentId, quantity: Quantity) -> Result<Self> {
        let held = self.inventory.get(&instrument).copied().unwrap_or_default();
        self.inventory
            .insert(instrument, exact::add(held, quantity.value())?);
        Ok(self)
    }

    /// The venue.
    #[must_use]
    pub fn venue(&self) -> &VenueId {
        &self.venue
    }

    /// Cash not reserved.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn available_cash(&self) -> Result<Money> {
        self.cash.checked_sub(self.reserved_cash)
    }

    /// Cash reserved so far.
    #[must_use]
    pub fn reserved_cash(&self) -> Money {
        self.reserved_cash
    }

    /// Inventory of `instrument` not reserved.
    ///
    /// # Errors
    ///
    /// Arithmetic errors.
    pub fn available_inventory(&self, instrument: &InstrumentId) -> Result<Decimal> {
        let held = self.inventory.get(instrument).copied().unwrap_or_default();
        let reserved = self
            .reserved_inventory
            .get(instrument)
            .copied()
            .unwrap_or_default();
        exact::sub(held, reserved)
    }

    /// Releases a reservation made on this venue (for example when the leg
    /// is cancelled unfilled).
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInput`] if the reservation is for another
    /// venue or exceeds what is reserved; arithmetic errors.
    pub fn release(&mut self, reservation: &Reservation) -> Result<()> {
        if reservation.venue != self.venue {
            return Err(ArbitrageError::InvalidInput {
                reason: "reservation belongs to another venue",
            });
        }
        let cash = self.reserved_cash.checked_sub(reservation.cash)?;
        if cash.is_negative() {
            return Err(ArbitrageError::InvalidInput {
                reason: "releasing more cash than is reserved",
            });
        }
        if let Some((instrument, quantity)) = &reservation.inventory {
            let reserved = self
                .reserved_inventory
                .get(instrument)
                .copied()
                .unwrap_or_default();
            let left = exact::sub(reserved, quantity.value())?;
            if left < Decimal::ZERO {
                return Err(ArbitrageError::InvalidInput {
                    reason: "releasing more inventory than is reserved",
                });
            }
            self.reserved_inventory.insert(instrument.clone(), left);
        }
        self.reserved_cash = cash;
        Ok(())
    }

    /// Reserves `need` if it fits, all or nothing; otherwise returns why not
    /// and changes nothing.
    pub(crate) fn reserve(&mut self, need: &Reservation) -> Result<Option<AdmissionReject>> {
        let available = self.available_cash()?;
        if need.cash.checked_sub(available)?.is_positive() {
            return Ok(Some(AdmissionReject::InsufficientCash {
                required: need.cash,
                available,
            }));
        }
        if let Some((instrument, quantity)) = &need.inventory {
            let available = self.available_inventory(instrument)?;
            if quantity.value() > available {
                return Ok(Some(AdmissionReject::InsufficientInventory {
                    required: *quantity,
                    available,
                }));
            }
            let reserved = self
                .reserved_inventory
                .get(instrument)
                .copied()
                .unwrap_or_default();
            self.reserved_inventory
                .insert(instrument.clone(), exact::add(reserved, quantity.value())?);
        }
        self.reserved_cash = self.reserved_cash.checked_add(need.cash)?;
        Ok(None)
    }
}

/// What one leg reserved on its venue.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reservation {
    /// The venue the capital is reserved on.
    pub venue: VenueId,
    /// Cash reserved.
    pub cash: Money,
    /// Inventory reserved, if the leg sells from inventory.
    pub inventory: Option<(InstrumentId, Quantity)>,
}

/// One leg as presented for admission.
#[derive(Debug, Clone, Copy)]
pub struct LegRequest<'a> {
    /// The leg.
    pub spec: &'a LegSpec,
    /// Its venue's latest view of the leg's instrument.
    pub view: &'a VenueView,
    /// Taker fee on that venue.
    pub fee: FeeRate,
    /// How a sell is funded (ignored for a buy).
    pub sell_funding: SellFunding,
}

/// How a leg pays on its venue: its taker fee and, for a sell, its funding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LegFunding {
    /// Taker fee on the leg's venue.
    pub fee: FeeRate,
    /// How a sell is funded (ignored for a buy).
    pub sell_funding: SellFunding,
}

/// One admitted leg, as handed to
/// [`TwoLegPlan::with_budget_gating`](crate::legs::TwoLegPlan::with_budget_gating):
/// what [`admit_two_legs`] was given and what it reserved for the leg.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AdmittedLeg {
    /// The leg's fee and funding, as given to the admission.
    pub funding: LegFunding,
    /// The view the leg was admitted on.
    pub view: VenueView,
    /// The reservation the admission made for the leg. It is already held
    /// in the venue's budget: handing it over never reserves it again.
    pub reservation: Reservation,
}

/// Why a leg is not admitted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AdmissionReject {
    /// The view is older than the allowed staleness.
    Stale {
        /// Its age at the decision, in milliseconds.
        age_ms: i64,
        /// The allowed staleness.
        max_staleness_ms: i64,
    },
    /// The leg's effective deadline (own deadline or instrument expiry) is
    /// at or before the decision.
    PastDeadline,
    /// The side the leg trades is not quoted.
    NoQuote,
    /// The executable price is beyond the leg's limit.
    PriceBeyondLimit,
    /// Less size is quoted than the leg needs.
    InsufficientSize {
        /// Quoted size.
        available: Quantity,
    },
    /// The venue has no budget for this account.
    NoBudget,
    /// Not enough unreserved cash on the venue.
    InsufficientCash {
        /// Cash the leg needs.
        required: Money,
        /// Unreserved cash on the venue.
        available: Money,
    },
    /// Not enough unreserved inventory on the venue.
    InsufficientInventory {
        /// Quantity the leg sells.
        required: Quantity,
        /// Unreserved inventory on the venue.
        available: Decimal,
    },
    /// What the leg's fills use could not be computed exactly (a product
    /// that does not fit a `Decimal`), so the capital a resubmission needs
    /// is unknown and none is sent.
    CoverageUnknown,
    /// The plan's reservations were released
    /// ([`TwoLegPlan::release_reservations`](crate::legs::TwoLegPlan::release_reservations)):
    /// it reserves nothing more, so no resubmission is sent.
    Released,
}

/// Result of [`admit_two_legs`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Admission {
    /// Both legs passed and their capital is reserved.
    Admitted {
        /// The first leg's reservation.
        first: Reservation,
        /// The second leg's reservation.
        second: Reservation,
    },
    /// A leg did not pass; nothing was reserved.
    Rejected {
        /// The leg that did not pass (the first one checked).
        leg: LegIndex,
        /// Why.
        reason: AdmissionReject,
    },
}

/// Checks one leg's view at `as_of` without reserving anything.
///
/// # Errors
///
/// - [`ArbitrageError::UnsupportedAction`] if the leg's side is not buy or sell
///   (checked first).
/// - [`ArbitrageError::InvalidInput`] if the view is for another instrument
///   or another venue than the leg's.
/// - [`ArbitrageError::Expired`] if the leg's instrument has expired.
/// - [`ArbitrageError::LookAhead`] if the view is observed after `as_of`.
pub fn check_view(
    spec: &LegSpec,
    view: &VenueView,
    as_of: Timestamp,
    max_staleness_ms: i64,
) -> Result<Option<AdmissionReject>> {
    Dir::try_from(spec.leg.side)?;
    if view.instrument.id != spec.leg.instrument.id {
        return Err(ArbitrageError::InvalidInput {
            reason: "venue view is for another instrument",
        });
    }
    if spec.venue != view.venue {
        return Err(ArbitrageError::InvalidInput {
            reason: "venue view is from another venue than the leg's",
        });
    }
    spec.leg.instrument.ensure_live(as_of)?;
    if spec.effective_deadline().is_some_and(|d| d <= as_of) {
        return Ok(Some(AdmissionReject::PastDeadline));
    }
    let age_ms = view.age_ms(as_of)?;
    if age_ms > max_staleness_ms {
        return Ok(Some(AdmissionReject::Stale {
            age_ms,
            max_staleness_ms,
        }));
    }
    let Some(level) = view.book.executable(spec.leg.side)? else {
        return Ok(Some(AdmissionReject::NoQuote));
    };
    let within_limit = match Dir::try_from(spec.leg.side)? {
        Dir::Buy => level.price <= spec.leg.price,
        Dir::Sell => level.price >= spec.leg.price,
    };
    if !within_limit {
        return Ok(Some(AdmissionReject::PriceBeyondLimit));
    }
    if level.size < spec.leg.quantity {
        return Ok(Some(AdmissionReject::InsufficientSize {
            available: level.size,
        }));
    }
    Ok(None)
}

/// Admits both legs of a plan at `as_of`: checks each view and reserves each
/// leg's capital on its own venue's budget, all or nothing.
///
/// # Errors
///
/// - [`ArbitrageError::UnsupportedAction`] if either leg's side is not buy or
///   sell, before any leg is rejected.
/// - The errors of [`check_view`].
/// - [`ArbitrageError::CurrencyMismatch`] if a requirement and the budget
///   are in different currencies.
/// - The errors of [`requirement`].
pub fn admit_two_legs(
    first: LegRequest<'_>,
    second: LegRequest<'_>,
    budgets: &mut BTreeMap<VenueId, VenueBudget>,
    as_of: Timestamp,
    max_staleness_ms: i64,
) -> Result<Admission> {
    // Both directions before either leg can be rejected for another reason.
    Dir::try_from(first.spec.leg.side)?;
    Dir::try_from(second.spec.leg.side)?;
    let mut next = budgets.clone();
    let mut reserved = Vec::with_capacity(2);
    for (leg, request) in [(LegIndex::First, first), (LegIndex::Second, second)] {
        if let Some(reason) = check_view(request.spec, request.view, as_of, max_staleness_ms)? {
            return Ok(Admission::Rejected { leg, reason });
        }
        // check_view guarantees the view is from the leg's venue.
        let Some(budget) = next.get_mut(&request.view.venue) else {
            return Ok(Admission::Rejected {
                leg,
                reason: AdmissionReject::NoBudget,
            });
        };
        let need = requirement(&request, budget.cash.currency())?;
        if let Some(reason) = budget.reserve(&need)? {
            return Ok(Admission::Rejected { leg, reason });
        }
        reserved.push(need);
    }
    *budgets = next;
    let second = reserved.pop();
    let first = reserved.pop();
    match (first, second) {
        (Some(first), Some(second)) => Ok(Admission::Admitted { first, second }),
        _ => Err(ArbitrageError::InvalidInput {
            reason: "admission did not reserve both legs",
        }),
    }
}

/// The capital a leg needs on its venue, in `currency`.
///
/// A buy needs `notional + fee` in cash at its limit price (an upper bound);
/// a sell from inventory needs the quantity and the fee; a short sale on
/// margin needs `notional * rate + fee` in cash. A sell's notional uses the
/// higher of its limit and the view's executable bid, since a sell limit is
/// only a lower bound. A rebate (negative fee) never lowers the
/// requirement.
///
/// # Errors
///
/// - [`ArbitrageError::UnsupportedAction`] if the leg's side is not buy or
///   sell.
/// - [`ArbitrageError::InvalidValue`] for a margin rate outside `(0, 1]`.
/// - Arithmetic errors.
pub fn requirement(request: &LegRequest<'_>, ccy: Currency) -> Result<Reservation> {
    let price = price_bound(request.spec, request.view)?;
    let funding = LegFunding {
        fee: request.fee,
        sell_funding: request.sell_funding,
    };
    let mut need = requirement_at(request.spec, price, funding, ccy)?;
    need.venue = request.view.venue.clone();
    Ok(need)
}

/// The price a leg's requirement is valued at on `view`: the limit for a
/// buy (an upper bound on its price); for a sell, whose limit is only a
/// lower bound, the higher of its limit and the observed executable bid.
///
/// # Errors
///
/// [`ArbitrageError::UnsupportedAction`] if the leg's side is not buy or
/// sell.
pub(crate) fn price_bound(spec: &LegSpec, view: &VenueView) -> Result<Price> {
    let leg = &spec.leg;
    Ok(
        match (
            Dir::try_from(leg.side)?,
            view.book.executable(Action::Sell)?,
        ) {
            (Dir::Sell, Some(bid)) => leg.price.max(bid.price),
            (Dir::Sell, None) | (Dir::Buy, _) => leg.price,
        },
    )
}

/// The capital the whole leg needs on its own venue with its notional
/// valued at `price`, in `ccy` (see [`requirement`]). It is non-decreasing
/// in `price`: the notional, the margin and a non-negative fee grow with it,
/// and a rebate is floored at zero.
///
/// # Errors
///
/// As [`requirement`].
pub(crate) fn requirement_at(
    spec: &LegSpec,
    price: Price,
    funding: LegFunding,
    ccy: Currency,
) -> Result<Reservation> {
    let leg = &spec.leg;
    let (px, q) = (price.value(), leg.quantity.value());
    let fee = funding.fee.fee_on_factors(&[px, q], ccy)?.money();
    let fee = if fee.is_negative() {
        Money::zero(ccy)
    } else {
        fee
    };
    let (cash, inventory) = match (Dir::try_from(leg.side)?, funding.sell_funding) {
        (Dir::Buy, _) => (
            Money::new(
                wide::sum_products_exact(&[&[px, q][..], &[fee.amount()]], "requirement")?,
                ccy,
            ),
            None,
        ),
        (Dir::Sell, SellFunding::Inventory) => {
            (fee, Some((leg.instrument.id.clone(), leg.quantity)))
        }
        (Dir::Sell, SellFunding::Margin { rate }) => {
            if rate <= Decimal::ZERO || rate > Decimal::ONE {
                return Err(ArbitrageError::InvalidValue {
                    field: "margin rate",
                    value: rate,
                    reason: "must be within (0, 1]",
                });
            }
            // rate * price * quantity + fee as one exact sum: the notional
            // may pass the range of a Decimal when the margin does not.
            (
                Money::new(
                    wide::sum_products_exact(
                        &[&[rate, px, q][..], &[fee.amount()]],
                        "requirement",
                    )?,
                    ccy,
                ),
                None,
            )
        }
    };
    Ok(Reservation {
        venue: spec.venue.clone(),
        cash,
        inventory,
    })
}
