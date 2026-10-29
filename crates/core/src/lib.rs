//! Core types and money math shared by every crate. No floats for money.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub mod book;
pub mod math;
pub use book::{BookLevel, OrderBook};
pub use math::{bps_of, edge_bps, fee_amount, net_edge_bps};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    Binance,
    Bybit,
    Okx,
    Indodax,
    Kraken,
    Uniswap,
    Pancakeswap,
    Jupiter,
}

impl Venue {
    pub fn is_cex(&self) -> bool {
        matches!(
            self,
            Self::Binance | Self::Bybit | Self::Okx | Self::Indodax | Self::Kraken
        )
    }
    pub fn is_dex(&self) -> bool {
        !self.is_cex()
    }
    /// Taker fee in bps (all strategies take liquidity). VIP0 spot
    /// schedules: Binance/Bybit/OKX all 0.10%. Indodax stacks the 0.20%
    /// pair fee (live /api/pairs) with PPh + CFX levies; 51bps holds a
    /// small conservative margin over that stack. Kraken tier-1 spot is
    /// 0.80% taker (fee schedule, verified 2026-10-29; BTC/USDT uses the
    /// crypto-spot schedule since the stablecoin is the quote, not base).
    pub fn default_taker_bps(&self) -> Decimal {
        match self {
            Self::Binance => Decimal::new(10, 2),
            Self::Bybit => Decimal::new(10, 2),
            Self::Okx => Decimal::new(10, 2),
            Self::Indodax => Decimal::new(51, 2),
            Self::Kraken => Decimal::new(80, 2),
            Self::Uniswap => Decimal::new(30, 2), // 0.30% pool tier default
            Self::Pancakeswap => Decimal::new(25, 2), // 0.25%
            Self::Jupiter => Decimal::ZERO,       // fee lives inside the quoted route
        }
    }

    /// Minimum order notional in quote terms. Verified 2026-10-27 against
    /// each venue's live instrument API (Binance NOTIONAL filter, Bybit
    /// minOrderAmt, OKX minSz at market, Indodax Rp25k Pro minimum).
    /// shortcut: Binance varies per symbol (1-5 seen), OKX is per-instrument
    /// (~$1); upgrade to a per-symbol table if sizing ever nears the floor.
    pub fn min_notional_quote(&self) -> Decimal {
        match self {
            Self::Binance => Decimal::new(5, 0),
            Self::Bybit => Decimal::new(5, 0),
            Self::Okx => Decimal::ONE,
            Self::Indodax => Decimal::new(15, 1), // Rp25k at ~16.7k/USD
            // Kraken `costmin` for XBT/USDT is 0.5 quote units (AssetPairs,
            // verified 2026-10-29). Per-pair values vary; same shortcut as above.
            Self::Kraken => Decimal::new(5, 1),
            Self::Uniswap | Self::Pancakeswap | Self::Jupiter => Decimal::ZERO,
        }
    }
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Binance => "binance",
            Self::Bybit => "bybit",
            Self::Okx => "okx",
            Self::Indodax => "indodax",
            Self::Kraken => "kraken",
            Self::Uniswap => "uniswap",
            Self::Pancakeswap => "pancakeswap",
            Self::Jupiter => "jupiter",
        };
        f.write_str(s)
    }
}

/// Trading pair like BTC/USDT. Quote is the numeraire leg.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Symbol {
    pub base: String,
    pub quote: String,
}

impl Symbol {
    pub fn new(base: &str, quote: &str) -> Self {
        Self {
            base: base.to_uppercase(),
            quote: quote.to_uppercase(),
        }
    }
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let (b, q) = s
            .split_once('/')
            .ok_or_else(|| CoreError::BadSymbol(s.into()))?;
        if b.is_empty() || q.is_empty() {
            return Err(CoreError::BadSymbol(s.into()));
        }
        Ok(Self::new(b, q))
    }
    /// Venue-native spellings.
    pub fn binance(&self) -> String {
        format!("{}{}", self.base, self.quote).to_lowercase()
    }
    pub fn bybit(&self) -> String {
        format!("{}{}", self.base, self.quote)
    }
    pub fn okx(&self) -> String {
        format!("{}-{}", self.base, self.quote)
    }
    pub fn indodax(&self) -> String {
        format!("{}{}", self.base, self.quote).to_lowercase()
    }
    /// Kraken WS v2 spelling: slash-separated, `BTC` not `XBT`
    /// (verified live 2026-10-29: `BTC/USD` and `BTC/USDT` subscribe).
    pub fn kraken(&self) -> String {
        format!("{}/{}", self.base, self.quote)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.quote)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub venue: Venue,
    pub symbol: Symbol,
    pub bid: Decimal,
    pub ask: Decimal,
    pub bid_qty: Decimal,
    pub ask_qty: Decimal,
    /// Milliseconds since epoch when the venue produced this quote.
    pub ts_ms: i64,
}

impl Quote {
    pub fn mid(&self) -> Decimal {
        (self.bid + self.ask) / Decimal::TWO
    }
    pub fn is_stale(&self, now_ms: i64, max_age_ms: i64) -> bool {
        now_ms - self.ts_ms > max_age_ms
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeLeg {
    pub venue: Venue,
    pub symbol: Symbol,
    pub side: Side,
    pub price: Decimal,
    pub qty_base: Decimal,
    /// Spot vs perpetual. Funding entries pair a Spot buy with a Perp sell;
    /// executors must never guess this from the symbol string.
    #[serde(default)]
    pub market: MarketKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarketKind {
    #[default]
    Spot,
    Perp,
}

impl TradeLeg {
    pub fn notional_quote(&self) -> Decimal {
        self.price * self.qty_base
    }
}

fn default_true() -> bool {
    true
}

/// A multi-leg trade intent emitted by a strategy. Executor owns placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub strategy: String,
    pub legs: Vec<TradeLeg>,
    pub expected_edge_bps: Decimal,
    pub ttl_ms: i64,
    pub created_ms: i64,
    /// True when legs close out immediately (arb round-trip; PnL known at fill).
    /// False for position-opening intents like funding-rate entries, whose PnL
    /// accrues over time and must NOT be marked as an instant round-trip.
    #[serde(default = "default_true")]
    pub roundtrip: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub venue: Venue,
    pub symbol: Symbol,
    pub side: Side,
    pub price: Decimal,
    pub qty_base: Decimal,
    pub fee_quote: Decimal,
    pub ts_ms: i64,
    pub simulated: bool,
    #[serde(default)]
    pub market: MarketKind,
}

/// Balances per venue: asset -> free amount.
pub type Balances = BTreeMap<Venue, BTreeMap<String, Decimal>>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("bad symbol: {0} (expected BASE/QUOTE)")]
    BadSymbol(String),
    #[error("empty book side")]
    EmptyBook,
    #[error("insufficient depth: need {need}, have {have}")]
    InsufficientDepth { need: Decimal, have: Decimal },
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn symbol_spellings() {
        let s = Symbol::parse("btc/usdt").unwrap();
        assert_eq!(s.binance(), "btcusdt");
        assert_eq!(s.okx(), "BTC-USDT");
        assert_eq!(s.kraken(), "BTC/USDT");
        assert_eq!(s.to_string(), "BTC/USDT");
        assert!(Symbol::parse("BTCUSDT").is_err());
    }

    #[test]
    fn quote_mid_and_stale() {
        let q = Quote {
            venue: Venue::Binance,
            symbol: Symbol::new("BTC", "USDT"),
            bid: dec!(100),
            ask: dec!(102),
            bid_qty: dec!(1),
            ask_qty: dec!(1),
            ts_ms: 1_000,
        };
        assert_eq!(q.mid(), dec!(101));
        assert!(q.is_stale(2_000, 500));
        assert!(!q.is_stale(1_400, 500));
    }
}
