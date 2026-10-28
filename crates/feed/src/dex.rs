//! Polling DEX quote feeds (HTTP reads only, no sends).
//!
//! Two sources, both fail-closed: Jupiter is a public HTTP API keyed off the
//! engine's symbol list; the EVM V2 poller needs an RPC URL plus the pool
//! address in config, and stays off while either is empty. Quotes are
//! timestamped at fetch; `cex_dex` drops any quote older than
//! `dex_max_quote_age_ms`, so a stalled poller degrades to no quotes, never
//! to stale fills.

use arb_core::{now_ms, Symbol, Venue};
use arb_dex_evm::{to_dex_quote as evm_to_dex_quote, DexKind, V2Pool};
use arb_dex_sol::{
    buy_price as jup_buy, parse_quote as jup_parse, quote_url as jup_url, sell_price as jup_sell,
};
use arb_strategy::DexQuote;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Latest polled DEX quotes, keyed by (venue, `"BASE/QUOTE"`).
pub type LiveDexQuotes = Arc<Mutex<BTreeMap<(Venue, String), DexQuote>>>;

/// Decode an `eth_call` `getReserves()` result (`0x` + three 32-byte words:
/// reserve0, reserve1, blockTimestampLast) into raw integer reserves.
pub fn decode_reserves(raw: &str) -> Option<(u128, u128)> {
    let h = raw.strip_prefix("0x").unwrap_or(raw);
    if h.len() < 128 {
        return None;
    }
    let r0 = u128::from_str_radix(&h[0..64], 16).ok()?;
    let r1 = u128::from_str_radix(&h[64..128], 16).ok()?;
    Some((r0, r1))
}

/// Raw integer reserve to human units at `decimals` places.
pub fn human_amount(raw: u128, decimals: u32) -> Decimal {
    let amount: Decimal = raw.to_string().parse().unwrap_or_default();
    let mut scale = Decimal::ONE;
    for _ in 0..decimals {
        scale *= Decimal::TEN;
    }
    amount / scale
}

fn http_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .build()
        .expect("reqwest client builds")
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())
}

async fn post_json(client: &reqwest::Client, url: &str, body: &Value) -> Result<Value, String> {
    let text = client
        .post(url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(body).map_err(|e| e.to_string())?)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// One `eth_call` `getReserves()` (`0x0902f1ac`) against the pool.
async fn evm_reserves(
    client: &reqwest::Client,
    rpc_url: &str,
    pool: &str,
) -> Result<(u128, u128), String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_call",
        "params": [{ "to": pool, "data": "0x0902f1ac" }, "latest"],
    });
    let v = post_json(client, rpc_url, &body).await?;
    let hex = v["result"]
        .as_str()
        .ok_or_else(|| "no result in RPC reply".to_string())?;
    decode_reserves(hex).ok_or_else(|| "bad getReserves blob".into())
}

#[derive(Debug, Clone)]
pub struct JupiterCfg {
    pub symbols: Vec<Symbol>,
    pub target_notional_quote: Decimal,
    pub gas_quote: Decimal,
    pub poll_secs: u64,
}

#[derive(Debug, Clone)]
pub struct EvmCfg {
    pub symbol: Symbol,
    pub rpc_url: String,
    pub pool: String,
    /// True when the pair's token0 is the base asset (e.g. WETH in
    /// ETH/USDT). A wrong setting inverts every quote, which shows up
    /// immediately as inverted paper edges — fail-loud, not fail-silent.
    pub token0_is_base: bool,
    pub base_decimals: u32,
    pub quote_decimals: u32,
    pub gas_quote: Decimal,
    pub target_notional_quote: Decimal,
    pub poll_secs: u64,
}

fn put(shared: &LiveDexQuotes, q: DexQuote) {
    let mut map = shared.lock().unwrap_or_else(|e| e.into_inner());
    map.insert((q.venue, q.symbol.to_string()), q);
}

/// Poll Jupiter for both directions of each symbol. Amounts track
/// `target_notional_quote` around the last mid so the quoted size stays near
/// what the strategy sweeps; the first poll assumes $100/base.
pub fn spawn_jupiter_task(shared: LiveDexQuotes, cfg: JupiterCfg) {
    tokio::spawn(async move {
        let client = http_client();
        let mut mids: BTreeMap<String, Decimal> = BTreeMap::new();
        loop {
            for sym in &cfg.symbols {
                let mid = mids
                    .get(&sym.to_string())
                    .copied()
                    .unwrap_or(Decimal::new(100, 0));
                let qty_base = if mid > Decimal::ZERO {
                    cfg.target_notional_quote / mid
                } else {
                    Decimal::ONE
                };
                let now = now_ms();
                let sell_q = match jup_url(sym, qty_base, true, 50) {
                    Some(u) => match get_text(&client, &u).await {
                        Ok(t) => jup_parse(&t).ok(),
                        Err(e) => {
                            tracing::warn!(symbol = %sym, %e, "jupiter sell quote failed");
                            None
                        }
                    },
                    None => None,
                };
                let buy_q = match jup_url(sym, cfg.target_notional_quote, false, 50) {
                    Some(u) => match get_text(&client, &u).await {
                        Ok(t) => jup_parse(&t).ok(),
                        Err(e) => {
                            tracing::warn!(symbol = %sym, %e, "jupiter buy quote failed");
                            None
                        }
                    },
                    None => None,
                };
                match (sell_q, buy_q) {
                    (Some(s), Some(b)) => {
                        if let (Some(sp), Some(bp)) = (jup_sell(&s, sym), jup_buy(&b, sym)) {
                            mids.insert(sym.to_string(), (sp + bp) / Decimal::TWO);
                            put(
                                &shared,
                                DexQuote {
                                    venue: Venue::Jupiter,
                                    symbol: sym.clone(),
                                    buy_px: bp,
                                    sell_px: sp,
                                    gas_quote: cfg.gas_quote,
                                    ts_ms: now,
                                },
                            );
                        }
                    }
                    _ => tracing::debug!(symbol = %sym, "jupiter quote incomplete; skipped"),
                }
            }
            tokio::time::sleep(Duration::from_secs(cfg.poll_secs.max(1))).await;
        }
    });
}

/// Poll one EVM V2 pool's reserves and quote `target_notional_quote` worth
/// at full size, so the strategy's smaller sweeps see a conservative price.
pub fn spawn_evm_task(shared: LiveDexQuotes, cfg: EvmCfg) {
    tokio::spawn(async move {
        let client = http_client();
        loop {
            match evm_reserves(&client, &cfg.rpc_url, &cfg.pool).await {
                Ok((r0, r1)) => {
                    let (rb, rq) = if cfg.token0_is_base {
                        (
                            human_amount(r0, cfg.base_decimals),
                            human_amount(r1, cfg.quote_decimals),
                        )
                    } else {
                        (
                            human_amount(r1, cfg.base_decimals),
                            human_amount(r0, cfg.quote_decimals),
                        )
                    };
                    let pool = V2Pool {
                        kind: DexKind::UniswapV2,
                        symbol: cfg.symbol.clone(),
                        reserve_base: rb,
                        reserve_quote: rq,
                        fee_bps: DexKind::UniswapV2.default_fee_bps(),
                    };
                    let spot = pool.spot_price();
                    if spot > Decimal::ZERO {
                        let qty = cfg.target_notional_quote / spot;
                        if let Some(q) = evm_to_dex_quote(&pool, qty, cfg.gas_quote, now_ms()) {
                            put(&shared, q);
                        }
                    }
                }
                Err(e) => tracing::warn!(pool = %cfg.pool, %e, "evm reserves poll failed"),
            }
            tokio::time::sleep(Duration::from_secs(cfg.poll_secs.max(1))).await;
        }
    });
}

/// Start both DEX pollers. `None` skips that source (fail-closed).
pub fn spawn_dex_tasks(shared: &LiveDexQuotes, jupiter: Option<JupiterCfg>, evm: Option<EvmCfg>) {
    if let Some(c) = jupiter {
        spawn_jupiter_task(shared.clone(), c);
    }
    if let Some(c) = evm {
        if c.rpc_url.trim().is_empty() || c.pool.trim().is_empty() {
            tracing::info!("evm dex polling off (no rpc url or pool)");
        } else {
            spawn_evm_task(shared.clone(), c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn reserves_blob_decodes_two_words() {
        // reserve0 = 100 ETH (1e20 wei), reserve1 = 300_000 USDT (3e11 base
        // units), then a timestamp word. Only the first two words matter.
        let blob = format!(
            "0x{:064x}{:064x}{:064x}",
            100_000_000_000_000_000_000u128, 300_000_000_000u128, 1_700_000_000u128
        );
        assert_eq!(
            decode_reserves(&blob),
            Some((100_000_000_000_000_000_000u128, 300_000_000_000u128))
        );
        assert_eq!(decode_reserves("0x1234"), None);
        assert_eq!(decode_reserves("nohex"), None);
    }

    #[test]
    fn human_amount_scales_by_decimals() {
        assert_eq!(human_amount(1_000_000_000_000_000_000, 18), dec!(1));
        assert_eq!(human_amount(300_000_000_000, 6), dec!(300000));
        assert_eq!(human_amount(0, 18), dec!(0));
    }
}
