//! Solana DEX via Jupiter Swap API: quote -> build -> sign/send with own RPC.
//!
//! This crate owns request building and quote parsing (pure, testable).
//! HTTP transport + transaction signing live in the engine's live wiring,
//! except the devnet-only [`testnet`] probe (HTTPS RPC only, off by default
//! so paper builds stay light).

pub mod testnet;

use arb_core::{Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

pub const JUPITER_QUOTE_URL: &str = "https://api.jup.ag/swap/v1/quote";
pub const JUPITER_SWAP_URL: &str = "https://api.jup.ag/swap/v1/swap";

// Well-known mints (mainnet).
pub const MINT_SOL: &str = "So11111111111111111111111111111111111111112";
pub const MINT_USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const MINT_USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

pub fn mint_for(asset: &str) -> Option<&'static str> {
    match asset.to_uppercase().as_str() {
        "SOL" | "WSOL" => Some(MINT_SOL),
        "USDC" => Some(MINT_USDC),
        "USDT" => Some(MINT_USDT),
        _ => None,
    }
}

pub fn decimals_for(asset: &str) -> u32 {
    match asset.to_uppercase().as_str() {
        "SOL" | "WSOL" => 9,
        _ => 6,
    }
}

/// Build the quote URL for swapping `amount_ui` of `symbol.base` into `symbol.quote`
/// (or reverse with `sell_base=false` meaning spend quote to buy base).
pub fn quote_url(
    symbol: &Symbol,
    amount_ui: Decimal,
    sell_base: bool,
    slippage_bps: u32,
) -> Option<String> {
    let (in_asset, out_asset) = if sell_base {
        (symbol.base.as_str(), symbol.quote.as_str())
    } else {
        (symbol.quote.as_str(), symbol.base.as_str())
    };
    let in_mint = mint_for(in_asset)?;
    let out_mint = mint_for(out_asset)?;
    let raw = amount_ui * Decimal::from(10u64.pow(decimals_for(in_asset)));
    let raw_int = raw.trunc();
    Some(format!("{JUPITER_QUOTE_URL}?inputMint={in_mint}&outputMint={out_mint}&amount={raw_int}&slippageBps={slippage_bps}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupQuote {
    pub in_amount_raw: Decimal,
    pub out_amount_raw: Decimal,
    pub price_impact_pct: Decimal,
    pub raw: Value,
}

fn dec_str(v: &Value) -> Decimal {
    match v {
        Value::String(s) => Decimal::from_str(s).unwrap_or_default(),
        Value::Number(n) => Decimal::from_str(&n.to_string()).unwrap_or_default(),
        _ => Decimal::ZERO,
    }
}

pub fn parse_quote(raw: &str) -> Result<JupQuote, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    Ok(JupQuote {
        in_amount_raw: dec_str(&v["inAmount"]),
        out_amount_raw: dec_str(&v["outAmount"]),
        price_impact_pct: dec_str(&v["priceImpactPct"]),
        raw: v,
    })
}

/// Effective quote-per-base sell price from a sell-base quote.
pub fn sell_price(q: &JupQuote, symbol: &Symbol) -> Option<Decimal> {
    if q.in_amount_raw <= Decimal::ZERO {
        return None;
    }
    let in_ui = q.in_amount_raw / Decimal::from(10u64.pow(decimals_for(&symbol.base)));
    let out_ui = q.out_amount_raw / Decimal::from(10u64.pow(decimals_for(&symbol.quote)));
    Some(out_ui / in_ui)
}

/// Effective quote-per-base buy price from a buy-base quote (spend quote,
/// receive base). Mirrors `sell_price` for the opposite direction.
pub fn buy_price(q: &JupQuote, symbol: &Symbol) -> Option<Decimal> {
    if q.out_amount_raw <= Decimal::ZERO {
        return None;
    }
    let in_ui = q.in_amount_raw / Decimal::from(10u64.pow(decimals_for(&symbol.quote)));
    let out_ui = q.out_amount_raw / Decimal::from(10u64.pow(decimals_for(&symbol.base)));
    Some(in_ui / out_ui)
}

pub fn to_dex_quote(
    symbol: &Symbol,
    sell: &JupQuote,
    buy_px: Decimal,
    gas_quote: Decimal,
    ts_ms: i64,
) -> Option<arb_strategy::DexQuote> {
    Some(arb_strategy::DexQuote {
        venue: Venue::Jupiter,
        symbol: symbol.clone(),
        buy_px,
        sell_px: sell_price(sell, symbol)?,
        gas_quote,
        ts_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn quote_url_and_parse() {
        let s = Symbol::new("SOL", "USDC");
        let u = quote_url(&s, dec!(1), true, 50).unwrap();
        assert!(u.contains("amount=1000000000"), "1 SOL = 1e9 lamports: {u}");
        assert!(u.contains(MINT_SOL) && u.contains(MINT_USDC));

        // Sold 1 SOL (1e9) -> got 150 USDC (150e6).
        let q = parse_quote(
            r#"{"inAmount":"1000000000","outAmount":"150000000","priceImpactPct":"0.01"}"#,
        )
        .unwrap();
        assert_eq!(sell_price(&q, &s), Some(dec!(150)));
        // Spent 150 USDC (150e6) -> got 1 SOL (1e9): buy price 150.
        let b = parse_quote(
            r#"{"inAmount":"150000000","outAmount":"1000000000","priceImpactPct":"0.01"}"#,
        )
        .unwrap();
        assert_eq!(buy_price(&b, &s), Some(dec!(150)));
        assert!(quote_url(&Symbol::new("DOGE", "USDC"), dec!(1), true, 50).is_none());
    }
}
