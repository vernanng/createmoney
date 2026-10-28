use arb_core::{Symbol, Venue};
use arb_strategy::StrategyParams;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub engine: EngineCfg,
    #[serde(default = "default_venues")]
    pub venues: Vec<Venue>,
    #[serde(default = "default_symbols")]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub strategies: StrategiesCfg,
    pub params: ParamsCfg,
    #[serde(default)]
    pub indodax: IndodaxCfg,
    #[serde(default)]
    pub paper: PaperCfg,
}

/// Starting simulated balances. Paper fills now enforce inventory: a sell of
/// an asset you do not hold (or a buy you cannot fund) aborts the intent.
#[derive(Debug, Deserialize)]
pub struct PaperCfg {
    #[serde(default = "default_start_quote")]
    pub start_quote_per_venue: Decimal,
    /// Optional per-venue quote override (`[paper.capital] binance = 10000`).
    /// Missing venues fall back to `start_quote_per_venue`.
    #[serde(default)]
    pub capital: BTreeMap<Venue, Decimal>,
    /// Withdrawal-style friction on virtual rebalances: fee in bps burned
    /// from the sender plus settlement delay in ms before crediting.
    #[serde(default)]
    pub transfer_fee_bps: Decimal,
    #[serde(default)]
    pub transfer_delay_ms: i64,
}
impl PaperCfg {
    pub fn quote_for(&self, venue: Venue) -> Decimal {
        self.capital
            .get(&venue)
            .copied()
            .unwrap_or(self.start_quote_per_venue)
    }
}
impl Default for PaperCfg {
    fn default() -> Self {
        Self {
            start_quote_per_venue: default_start_quote(),
            capital: BTreeMap::new(),
            transfer_fee_bps: Decimal::ZERO,
            transfer_delay_ms: 0,
        }
    }
}
fn default_start_quote() -> Decimal {
    Decimal::new(200, 0)
}

#[derive(Debug, Deserialize)]
pub struct EngineCfg {
    pub mode: String, // paper | live
    #[serde(default = "default_tick")]
    pub tick_ms: u64,
    #[serde(default)]
    pub api_addr: Option<String>,
    #[serde(default)]
    pub jsonl_path: Option<String>,
}

fn default_tick() -> u64 {
    250
}
fn default_venues() -> Vec<Venue> {
    vec![Venue::Binance, Venue::Bybit, Venue::Okx, Venue::Indodax]
}
fn default_symbols() -> Vec<String> {
    vec!["BTC/USDT".into(), "ETH/USDT".into()]
}

#[derive(Debug, Deserialize)]
pub struct StrategiesCfg {
    #[serde(default = "yes")]
    pub cross_exchange: bool,
    #[serde(default = "yes")]
    pub triangular: bool,
    #[serde(default = "yes")]
    pub funding_rate: bool,
    #[serde(default = "yes")]
    pub cex_dex: bool,
}
impl Default for StrategiesCfg {
    fn default() -> Self {
        Self {
            cross_exchange: true,
            triangular: true,
            funding_rate: true,
            cex_dex: true,
        }
    }
}
fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ParamsCfg {
    pub min_edge_bps: Decimal,
    pub slip_buffer_bps: Decimal,
    pub target_notional_quote: Decimal,
    pub max_book_age_ms: i64,
    #[serde(default = "default_ttl")]
    pub ttl_ms: i64,
    #[serde(default = "default_min_funding")]
    pub min_funding_annualized_bps: Decimal,
    /// S6 gate: None = off (legacy behavior). Some(t) requires both-sided
    /// imbalance agreement at threshold t before `cross_exchange` emits.
    #[serde(default)]
    pub imbalance_threshold: Option<Decimal>,
    /// Max quote one intent may spend on a single venue, in bps of that
    /// venue's USDT+USDC ledger. 0 = off. Default 2500 (25%): normal
    /// prints pass, S2-grown monsters that would eat a venue get skipped.
    #[serde(default = "default_max_venue_share")]
    pub max_venue_quote_share_bps: Decimal,
}
fn default_max_venue_share() -> Decimal {
    Decimal::new(2500, 0)
}
fn default_ttl() -> i64 {
    1_500
}
fn default_min_funding() -> Decimal {
    Decimal::new(500, 0)
}

impl ParamsCfg {
    pub fn to_strategy_params(&self) -> StrategyParams {
        StrategyParams {
            min_edge_bps: self.min_edge_bps,
            slip_buffer_bps: self.slip_buffer_bps,
            target_notional_quote: self.target_notional_quote,
            max_book_age_ms: self.max_book_age_ms,
            ttl_ms: self.ttl_ms,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct IndodaxCfg {
    /// Fallback USD/IDR reference for the FX-triangle strategy's own math.
    /// Book conversion itself always uses the streamed live rate
    /// (see `arb_feed::fx`) and withholds Indodax books when no live
    /// rate is acceptable.
    #[serde(default = "default_usdidr")]
    pub usd_idr: Decimal,
    /// Max age of the Indodax USDT/IDR book before it stops being a divisor.
    #[serde(default = "default_rate_max_age_ms")]
    pub rate_max_age_ms: i64,
    /// Max top-of-book spread of the USDT/IDR book, in bps of mid.
    #[serde(default = "default_rate_max_spread_bps")]
    pub rate_max_spread_bps: Decimal,
    /// Max deviation of the Indodax rate from the independent-source
    /// consensus (Upbit Indonesia + implied crosses), in bps. Beyond it,
    /// Indodax books are withheld instead of converted.
    #[serde(default = "default_rate_max_deviation_bps")]
    pub rate_max_deviation_bps: Decimal,
}
// Manual Default: `#[serde(default)]` on the parent field bypasses the
// per-field serde defaults when the whole [indodax] table is absent.
impl Default for IndodaxCfg {
    fn default() -> Self {
        Self {
            usd_idr: default_usdidr(),
            rate_max_age_ms: default_rate_max_age_ms(),
            rate_max_spread_bps: default_rate_max_spread_bps(),
            rate_max_deviation_bps: default_rate_max_deviation_bps(),
        }
    }
}
fn default_usdidr() -> Decimal {
    Decimal::new(16_000, 0)
}
fn default_rate_max_age_ms() -> i64 {
    30_000
}
fn default_rate_max_spread_bps() -> Decimal {
    Decimal::new(50, 0)
}
fn default_rate_max_deviation_bps() -> Decimal {
    Decimal::new(300, 0)
}

impl Config {
    pub fn symbols(&self) -> anyhow::Result<Vec<Symbol>> {
        self.symbols
            .iter()
            .map(|s| Symbol::parse(s).map_err(anyhow::Error::from))
            .collect()
    }
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_venues_and_symbols_are_honored() {
        // Regression: bare keys placed after a [table] header belong to that
        // table and are silently ignored — venues/symbols must be parsed.
        let text = r#"
venues = ["binance", "bybit"]
symbols = ["BTC/USDT", "SOL/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.venues, vec![Venue::Binance, Venue::Bybit]);
        let syms = cfg.symbols().unwrap();
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[1], Symbol::new("SOL", "USDT"));
        // No [indodax] table at all: live-rate guards still get real
        // defaults, not zeroes (serde field defaults do not run when the
        // whole table is defaulted).
        assert_eq!(cfg.indodax.rate_max_age_ms, 30_000);
        assert_eq!(cfg.indodax.rate_max_spread_bps, Decimal::new(50, 0));
        assert_eq!(cfg.indodax.rate_max_deviation_bps, Decimal::new(300, 0));
        assert_eq!(cfg.indodax.usd_idr, Decimal::new(16_000, 0));
    }

    #[test]
    fn imbalance_threshold_defaults_off_and_parses_when_set() {
        let bare = r#"
[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#;
        let cfg: Config = toml::from_str(bare).unwrap();
        assert_eq!(cfg.params.imbalance_threshold, None);
        let gated = r#"
[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
imbalance_threshold = 0.2
"#;
        let cfg: Config = toml::from_str(gated).unwrap();
        assert_eq!(cfg.params.imbalance_threshold, Some(Decimal::new(2, 1)));
    }

    #[test]
    fn per_venue_capital_overrides_fallback() {
        let text = r#"
[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000

[paper]
start_quote_per_venue = 200

[paper.capital]
binance = 10000
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.paper.quote_for(Venue::Binance), Decimal::new(10_000, 0));
        assert_eq!(cfg.paper.quote_for(Venue::Bybit), Decimal::new(200, 0));
        let bare = r#"
[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#;
        let cfg: Config = toml::from_str(bare).unwrap();
        assert_eq!(cfg.paper.quote_for(Venue::Binance), Decimal::new(200, 0));
    }
}
