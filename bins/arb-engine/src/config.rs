use arb_core::{Symbol, Venue};
use arb_risk::RiskLimits;
use arb_strategy::StrategyParams;
use rust_decimal::Decimal;
use serde::Deserialize;

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
    pub risk: RiskCfg,
    #[serde(default)]
    pub dex: DexCfg,
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
    #[serde(default = "default_start_base")]
    pub start_base_per_asset: Decimal,
}
impl Default for PaperCfg {
    fn default() -> Self {
        Self {
            start_quote_per_venue: default_start_quote(),
            start_base_per_asset: default_start_base(),
        }
    }
}
fn default_start_quote() -> Decimal {
    Decimal::new(1_000_000, 0)
}
fn default_start_base() -> Decimal {
    Decimal::new(100, 0)
}

#[derive(Debug, Deserialize)]
pub struct EngineCfg {
    pub mode: String, // paper | live
    pub feed: String, // synthetic | live
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
pub struct RiskCfg {
    pub max_notional_quote_per_trade: Decimal,
    pub max_notional_quote_per_venue: Decimal,
}
impl RiskCfg {
    pub fn to_limits(&self, min_edge_bps: Decimal, max_book_age_ms: i64) -> RiskLimits {
        RiskLimits {
            max_notional_quote_per_trade: self.max_notional_quote_per_trade,
            max_notional_quote_per_venue: self.max_notional_quote_per_venue,
            min_edge_bps,
            max_book_age_ms,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct DexCfg {
    #[serde(default = "yes")]
    pub uniswap: bool,
    #[serde(default = "yes")]
    pub pancakeswap: bool,
    #[serde(default = "yes")]
    pub jupiter: bool,
    /// Flat gas estimate in quote (USD) per EVM swap for paper quoting.
    #[serde(default = "default_gas")]
    pub evm_gas_quote: Decimal,
    #[serde(default = "default_gas")]
    pub solana_fee_quote: Decimal,
}
fn default_gas() -> Decimal {
    Decimal::new(1, 0)
}

#[derive(Debug, Deserialize)]
pub struct IndodaxCfg {
    /// Synthetic-feed reference for IDR books. Live mode never uses this:
    /// it converts with the streamed USDT/IDR rate (see `arb_feed::fx`)
    /// and withholds Indodax books when no live rate is acceptable.
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
feed = "synthetic"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000

[risk]
max_notional_quote_per_trade = 5000
max_notional_quote_per_venue = 25000
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
}
