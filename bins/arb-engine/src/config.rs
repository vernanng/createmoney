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

#[derive(Debug, Deserialize, Default)]
pub struct IndodaxCfg {
    /// USD/IDR reference for normalizing IDR books. Update daily or feed live.
    #[serde(default = "default_usdidr")]
    pub usd_idr: Decimal,
}
fn default_usdidr() -> Decimal {
    Decimal::new(16_000, 0)
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
