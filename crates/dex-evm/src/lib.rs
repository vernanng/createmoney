//! EVM DEX quoting for Uniswap and PancakeSwap.
//!
//! V2-style pools use exact constant-product math locally (no RPC needed once
//! reserves are known). V3 pools need tick math on-chain, so this crate
//! exposes the quoter interface types + well-known router/quoter addresses;
//! live Sepolia sends go through the chain-gated [`testnet`] probe (HTTPS
//! RPC only, off by default so paper builds stay light).

pub mod testnet;

use arb_core::{Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Well-known mainnet addresses (checksummed hex, verify before live use).
pub mod addr {
    // Ethereum mainnet
    pub const UNISWAP_V2_ROUTER: &str = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D";
    pub const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
    pub const UNISWAP_V3_ROUTER: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";
    // BNB Chain
    pub const PANCAKE_V2_ROUTER: &str = "0x10ED43C718714eb63d5aA57B78B54704E256024E";
    pub const PANCAKE_V3_QUOTER: &str = "0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DexKind {
    UniswapV2,
    UniswapV3,
    PancakeV2,
    PancakeV3,
}

impl DexKind {
    pub fn venue(&self) -> Venue {
        match self {
            Self::UniswapV2 | Self::UniswapV3 => Venue::Uniswap,
            Self::PancakeV2 | Self::PancakeV3 => Venue::Pancakeswap,
        }
    }
    /// Pool fee in bps. V3 defaults to the 0.05% tier for stables / 0.30% majors —
    /// caller overrides per pool.
    pub fn default_fee_bps(&self) -> Decimal {
        match self {
            Self::UniswapV2 => Decimal::new(30, 0),
            Self::PancakeV2 => Decimal::new(25, 0),
            Self::UniswapV3 | Self::PancakeV3 => Decimal::new(5, 0),
        }
    }
    pub fn router(&self) -> &'static str {
        match self {
            Self::UniswapV2 => addr::UNISWAP_V2_ROUTER,
            Self::UniswapV3 => addr::UNISWAP_V3_ROUTER,
            Self::PancakeV2 => addr::PANCAKE_V2_ROUTER,
            Self::PancakeV3 => addr::PANCAKE_V3_QUOTER,
        }
    }
}

/// V2 pool state in human (decimal-adjusted) units.
#[derive(Debug, Clone)]
pub struct V2Pool {
    pub kind: DexKind,
    pub symbol: Symbol,
    pub reserve_base: Decimal,
    pub reserve_quote: Decimal,
    pub fee_bps: Decimal,
}

impl V2Pool {
    /// Exact `getAmountOut`: out = (in*(1-f) * reserve_out) / (reserve_in + in*(1-f)).
    fn amount_out(
        amount_in: Decimal,
        reserve_in: Decimal,
        reserve_out: Decimal,
        fee_bps: Decimal,
    ) -> Decimal {
        if amount_in <= Decimal::ZERO || reserve_in <= Decimal::ZERO || reserve_out <= Decimal::ZERO
        {
            return Decimal::ZERO;
        }
        let in_net = amount_in * (Decimal::ONE - fee_bps / Decimal::new(10_000, 0));
        (in_net * reserve_out) / (reserve_in + in_net)
    }

    /// Price (quote per base) to BUY `qty_base` base, incl. impact + fee.
    /// Computes required quote-in for exact base-out via inverse formula.
    pub fn buy_price(&self, qty_base: Decimal) -> Option<Decimal> {
        if qty_base <= Decimal::ZERO || qty_base >= self.reserve_base {
            return None;
        }
        // in = (reserve_in * out) / ((reserve_out - out) * (1-f))
        let f = Decimal::ONE - self.fee_bps / Decimal::new(10_000, 0);
        let quote_in = (self.reserve_quote * qty_base) / ((self.reserve_base - qty_base) * f);
        Some(quote_in / qty_base)
    }

    /// Price (quote per base) received SELLING `qty_base` base.
    pub fn sell_price(&self, qty_base: Decimal) -> Option<Decimal> {
        if qty_base <= Decimal::ZERO {
            return None;
        }
        let quote_out = Self::amount_out(
            qty_base,
            self.reserve_base,
            self.reserve_quote,
            self.fee_bps,
        );
        Some(quote_out / qty_base)
    }

    pub fn spot_price(&self) -> Decimal {
        if self.reserve_base <= Decimal::ZERO {
            Decimal::ZERO
        } else {
            self.reserve_quote / self.reserve_base
        }
    }
}

/// Convert a V2 pool into a strategy `DexQuote` for a target base qty.
pub fn to_dex_quote(
    pool: &V2Pool,
    qty_base: Decimal,
    gas_quote: Decimal,
    ts_ms: i64,
) -> Option<arb_strategy::DexQuote> {
    Some(arb_strategy::DexQuote {
        venue: pool.kind.venue(),
        symbol: pool.symbol.clone(),
        buy_px: pool.buy_price(qty_base)?,
        sell_px: pool.sell_price(qty_base)?,
        gas_quote,
        ts_ms,
    })
}

/// V3 quote request (sent to the on-chain Quoter via eth_call in live mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V3QuoteRequest {
    pub kind: DexKind,
    pub token_in: String,
    pub token_out: String,
    pub fee_tier_ppm: u32, // 500, 3000, 10000 ...
    pub amount_in_wei: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn pool() -> V2Pool {
        // 100 ETH / 300_000 USDT -> spot 3000.
        V2Pool {
            kind: DexKind::UniswapV2,
            symbol: Symbol::new("ETH", "USDT"),
            reserve_base: dec!(100),
            reserve_quote: dec!(300000),
            fee_bps: dec!(30),
        }
    }

    #[test]
    fn v2_prices_bracket_spot_and_widen_with_size() {
        let p = pool();
        assert_eq!(p.spot_price(), dec!(3000));
        let buy_small = p.buy_price(dec!(0.1)).unwrap();
        let buy_big = p.buy_price(dec!(10)).unwrap();
        let sell_small = p.sell_price(dec!(0.1)).unwrap();
        assert!(buy_small > dec!(3000), "buy pays fee+impact: {buy_small}");
        assert!(sell_small < dec!(3000), "sell receives less: {sell_small}");
        assert!(buy_big > buy_small, "bigger trade, worse price");
        // Round-trip through the pool must lose money (fees + impact both ways).
        assert!(sell_small < buy_small);
    }

    #[test]
    fn pancake_fee_is_lower_than_uniswap_v2() {
        let mut cake = pool();
        cake.kind = DexKind::PancakeV2;
        cake.fee_bps = DexKind::PancakeV2.default_fee_bps();
        assert!(cake.sell_price(dec!(1)).unwrap() > pool().sell_price(dec!(1)).unwrap());
        assert_eq!(DexKind::PancakeV2.venue(), Venue::Pancakeswap);
        assert_eq!(DexKind::UniswapV3.venue(), Venue::Uniswap);
    }

    #[test]
    fn draining_the_pool_is_rejected() {
        assert!(pool().buy_price(dec!(100)).is_none());
        assert!(pool().buy_price(dec!(0)).is_none());
    }
}
