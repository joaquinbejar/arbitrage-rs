//! Instruments, times, sides and executable price levels.

use crate::error::{ArbitrageError, Result};
use crate::units::{Price, Quantity};
use core::fmt;

/// Maximum length of an [`InstrumentId`].
pub const MAX_INSTRUMENT_ID_LEN: usize = 64;

/// A point in time: milliseconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// A timestamp from milliseconds since the Unix epoch, UTC.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    /// Milliseconds since the Unix epoch, UTC.
    #[must_use]
    pub const fn millis(&self) -> i64 {
        self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

/// A venue's identifier for an instrument, such as `BTC-USD` or
/// `BTC-27DEC26-60000-C`.
///
/// 1 to 64 printable ASCII characters, no whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstrumentId(String);

impl InstrumentId {
    /// Builds an identifier.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::InvalidInstrument`] if `id` is empty, longer than 64
    /// characters or contains whitespace or non-printable characters.
    pub fn new(id: &str) -> Result<Self> {
        let valid = !id.is_empty()
            && id.len() <= MAX_INSTRUMENT_ID_LEN
            && id.bytes().all(|b| b.is_ascii_graphic());
        if valid {
            Ok(Self(id.to_owned()))
        } else {
            Err(ArbitrageError::InvalidInstrument { id: id.to_owned() })
        }
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An instrument: its identifier and, for dated products, its expiry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Instrument {
    /// Venue identifier.
    pub id: InstrumentId,
    /// Expiry of a dated future or option; `None` for spot and perpetuals.
    pub expiry: Option<Timestamp>,
}

impl Instrument {
    /// An undated instrument (spot or perpetual).
    #[must_use]
    pub fn undated(id: InstrumentId) -> Self {
        Self { id, expiry: None }
    }

    /// A dated instrument expiring at `expiry`.
    #[must_use]
    pub fn dated(id: InstrumentId, expiry: Timestamp) -> Self {
        Self {
            id,
            expiry: Some(expiry),
        }
    }

    /// Checks that the instrument has not expired at `as_of`.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::Expired`] if the expiry is at or before `as_of`.
    pub fn ensure_live(&self, as_of: Timestamp) -> Result<()> {
        match self.expiry {
            Some(expiry) if expiry <= as_of => Err(ArbitrageError::Expired {
                instrument: self.id.as_str().to_owned(),
                expiry,
                as_of,
            }),
            _ => Ok(()),
        }
    }
}

/// The trade direction of the public API: `financial_types::Action`,
/// shared with the other crates. Only [`Action::Buy`] (executed at the ask)
/// and [`Action::Sell`] (executed at the bid) are supported; any other
/// action is [`ArbitrageError::UnsupportedAction`].
pub use financial_types::Action;

/// The supported directions, internally. `Action` is a foreign
/// `#[non_exhaustive]` enum with an `Other` variant, so every input is
/// converted here once, the only place that needs a wildcard arm; all other
/// matches are exhaustive on `Dir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Dir {
    /// Buy, executed at the ask.
    Buy,
    /// Sell, executed at the bid.
    Sell,
}

impl TryFrom<Action> for Dir {
    type Error = ArbitrageError;

    fn try_from(action: Action) -> Result<Self> {
        match action {
            Action::Buy => Ok(Self::Buy),
            Action::Sell => Ok(Self::Sell),
            // `Other`, or a variant added in a later financial_types.
            action => Err(ArbitrageError::UnsupportedAction { action }),
        }
    }
}

impl From<Dir> for Action {
    fn from(dir: Dir) -> Self {
        match dir {
            Dir::Buy => Self::Buy,
            Dir::Sell => Self::Sell,
        }
    }
}

/// One executable price level: a price and the size available at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Level {
    /// Price of the level.
    pub price: Price,
    /// Size available at that price.
    pub size: Quantity,
}

impl Level {
    /// A level of `size` at `price`.
    #[must_use]
    pub fn new(price: Price, size: Quantity) -> Self {
        Self { price, size }
    }
}

/// Top of book of one instrument. Either side may be missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TopOfBook {
    /// Best bid, where a sell executes.
    pub bid: Option<Level>,
    /// Best ask, where a buy executes.
    pub ask: Option<Level>,
}

impl TopOfBook {
    /// The level a trade on `side` executes against: the ask for a buy, the
    /// bid for a sell; `None` if that side of the book is empty.
    ///
    /// # Errors
    ///
    /// [`ArbitrageError::UnsupportedAction`] for an action other than buy
    /// or sell (never confused with an empty book).
    pub fn executable(&self, side: Action) -> Result<Option<Level>> {
        Ok(match Dir::try_from(side)? {
            Dir::Buy => self.ask,
            Dir::Sell => self.bid,
        })
    }
}

/// One leg of an intended trade, at an executable price.
///
/// A leg is an intent: it states what would be sent, not what was filled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Leg {
    /// Instrument to trade.
    pub instrument: Instrument,
    /// Direction.
    pub side: Action,
    /// Size to trade.
    pub quantity: Quantity,
    /// Executable price the estimate uses (the ask for a buy, the bid for a
    /// sell); a natural limit price.
    pub price: Price,
}

#[cfg(test)]
// Tests fail loudly: the library's no-panic lints do not apply here.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn instrument_ids_are_validated() {
        assert_eq!(
            InstrumentId::new("BTC-27DEC26-60000-C").unwrap().as_str(),
            "BTC-27DEC26-60000-C"
        );
        let long = "X".repeat(MAX_INSTRUMENT_ID_LEN);
        assert!(InstrumentId::new(&long).is_ok());
        for bad in ["", "BTC USD", "BTC\tUSD", "BTC\u{e9}", &format!("{long}X")] {
            assert_eq!(
                InstrumentId::new(bad),
                Err(ArbitrageError::InvalidInstrument { id: bad.to_owned() })
            );
        }
    }

    #[test]
    fn expiry_check() {
        let id = InstrumentId::new("BTC-F").unwrap();
        let fut = Instrument::dated(id.clone(), Timestamp::from_millis(1_000));
        assert!(fut.ensure_live(Timestamp::from_millis(999)).is_ok());
        assert_eq!(
            fut.ensure_live(Timestamp::from_millis(1_000)),
            Err(ArbitrageError::Expired {
                instrument: "BTC-F".to_owned(),
                expiry: Timestamp::from_millis(1_000),
                as_of: Timestamp::from_millis(1_000),
            })
        );
        assert!(
            Instrument::undated(id)
                .ensure_live(Timestamp::from_millis(i64::MAX))
                .is_ok()
        );
    }

    #[test]
    fn executable_side() {
        let bid = Level::new(
            Price::new(dec!(99)).unwrap(),
            Quantity::new(dec!(1)).unwrap(),
        );
        let ask = Level::new(
            Price::new(dec!(101)).unwrap(),
            Quantity::new(dec!(2)).unwrap(),
        );
        let book = TopOfBook {
            bid: Some(bid),
            ask: Some(ask),
        };
        assert_eq!(book.executable(Action::Buy).unwrap(), Some(ask));
        assert_eq!(book.executable(Action::Sell).unwrap(), Some(bid));
        assert_eq!(TopOfBook::default().executable(Action::Buy).unwrap(), None);
        // Other is an error, never an empty side of the book.
        assert_eq!(
            book.executable(Action::Other),
            Err(ArbitrageError::UnsupportedAction {
                action: Action::Other
            })
        );
        assert_eq!(Action::Buy.opposite(), Action::Sell);
        assert_eq!(Action::Sell.to_string(), "Sell");
        for action in [Action::Buy, Action::Sell] {
            assert_eq!(Action::from(Dir::try_from(action).unwrap()), action);
        }
    }
}
