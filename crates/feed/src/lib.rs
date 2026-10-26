//! Market-data connectors. Parsers turn venue wire formats into `OrderBook`/`Quote`.
//! Live WS loops plug into these parsers; URLs and subscribe frames live here so
//! they are testable without a network.

use arb_core::{BookLevel, OrderBook, Quote, Symbol, Venue};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

pub mod synthetic;
pub use synthetic::SyntheticFeed;

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("bad payload for {venue}: {msg}")]
    BadPayload { venue: Venue, msg: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

fn dec(s: &str) -> Result<Decimal, FeedError> {
    Decimal::from_str(s).map_err(|e| FeedError::BadPayload {
        venue: Venue::Binance,
        msg: e.to_string(),
    })
}

fn dec_val(v: &Value) -> Result<Decimal, FeedError> {
    match v {
        Value::String(s) => dec(s),
        Value::Number(n) => Decimal::from_str(&n.to_string()).map_err(|e| FeedError::BadPayload {
            venue: Venue::Binance,
            msg: e.to_string(),
        }),
        _ => Err(FeedError::BadPayload {
            venue: Venue::Binance,
            msg: format!("not numeric: {v}"),
        }),
    }
}

fn levels(arr: &Value) -> Result<Vec<BookLevel>, FeedError> {
    let mut out = Vec::new();
    for row in arr.as_array().cloned().unwrap_or_default() {
        let r = row.as_array().cloned().unwrap_or_default();
        if r.len() >= 2 {
            out.push(BookLevel {
                price: dec_val(&r[0])?,
                qty: dec_val(&r[1])?,
            });
        }
    }
    Ok(out)
}

/// Public endpoints (verified against official docs; see README sources).
pub fn ws_url(venue: Venue) -> &'static str {
    match venue {
        Venue::Binance => "wss://stream.binance.com:9443",
        Venue::Bybit => "wss://stream.bybit.com/v5/public/spot",
        Venue::Okx => "wss://ws.okx.com:8443/ws/v5/public",
        Venue::Indodax => "wss://ws3.indodax.com/ws/",
        Venue::Uniswap | Venue::Pancakeswap | Venue::Jupiter => "",
    }
}

pub fn perp_ws_url(venue: Venue) -> &'static str {
    match venue {
        Venue::Bybit => "wss://stream.bybit.com/v5/public/linear",
        Venue::Binance => "wss://fstream.binance.com",
        Venue::Okx => "wss://ws.okx.com:8443/ws/v5/public",
        _ => "",
    }
}

/// Subscribe frame for a book/ticker stream. Indodax needs its token auth first
/// (see `indodax_auth_frame`); the token itself comes from config/env, never source.
pub fn subscribe_frame(venue: Venue, symbol: &Symbol) -> Value {
    match venue {
        Venue::Binance => serde_json::json!({
            "method": "SUBSCRIBE",
            "params": [format!("{}@depth@100ms", symbol.binance()), format!("{}@bookTicker", symbol.binance())],
            "id": 1
        }),
        Venue::Bybit => serde_json::json!({
            "op": "subscribe",
            "args": [format!("orderbook.50.{}", symbol.bybit()), format!("tickers.{}", symbol.bybit())]
        }),
        Venue::Okx => serde_json::json!({
            "op": "subscribe",
            "args": [
                {"channel": "books", "instId": symbol.okx()},
                {"channel": "tickers", "instId": symbol.okx()}
            ]
        }),
        Venue::Indodax => serde_json::json!({
            "method": 1,
            "params": {"channel": format!("market:orderbook-{}", symbol.indodax())},
            "id": 2
        }),
        _ => Value::Null,
    }
}

pub fn indodax_auth_frame(token: &str) -> Value {
    serde_json::json!({ "params": { "token": token }, "id": 1 })
}

// ---------- Binance ----------
// Diff depth: {"e":"depthUpdate","E":..,"s":"BTCUSDT","U":1,"u":2,"b":[[px,qty]],"a":[[px,qty]]}
pub fn parse_binance_depth(book: &mut OrderBook, raw: &str) -> Result<bool, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let first = v["U"].as_u64().unwrap_or(0);
    let fin = v["u"].as_u64().unwrap_or(0);
    let ts = v["E"].as_i64().unwrap_or(0);
    Ok(book.apply_diff(levels(&v["b"])?, levels(&v["a"])?, first, fin, ts))
}

// Book ticker: {"u":1,"s":"BTCUSDT","b":"100","B":"1","a":"101","A":"2"}
pub fn parse_binance_book_ticker(
    symbol: Symbol,
    raw: &str,
    ts_ms: i64,
) -> Result<Quote, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    Ok(Quote {
        venue: Venue::Binance,
        symbol,
        bid: dec_val(&v["b"])?,
        ask: dec_val(&v["a"])?,
        bid_qty: dec_val(&v["B"])?,
        ask_qty: dec_val(&v["A"])?,
        ts_ms,
    })
}

// ---------- Bybit V5 ----------
// {"topic":"orderbook.50.BTCUSDT","type":"snapshot|delta","ts":..,"data":{"s":"BTCUSDT","b":[[px,qty]],"a":[[..]],"u":1}}
pub fn parse_bybit_orderbook(book: &mut OrderBook, raw: &str) -> Result<bool, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let d = &v["data"];
    let ts = v["ts"].as_i64().unwrap_or(0);
    let seq = d["u"].as_u64().unwrap_or(0);
    let (bids, asks) = (levels(&d["b"])?, levels(&d["a"])?);
    if v["type"].as_str() == Some("snapshot") {
        book.apply_snapshot(bids, asks, seq, ts);
        Ok(true)
    } else {
        // Bybit deltas carry no first-id; treat sequence as both bounds.
        Ok(book.apply_diff(bids, asks, seq, seq, ts))
    }
}

// ---------- OKX v5 ----------
// {"arg":{"channel":"books","instId":"BTC-USDT"},"data":[{"bids":[[px,qty,..]],"asks":[[..]],"ts":"..","seqId":1}]}
pub fn parse_okx_books(book: &mut OrderBook, raw: &str) -> Result<bool, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let d = &v["data"][0];
    let ts: i64 = d["ts"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let seq = d["seqId"].as_u64().unwrap_or(0);
    let (bids, asks) = (levels(&d["bids"])?, levels(&d["asks"])?);
    if v["action"].as_str() == Some("snapshot") || book.last_update_id == 0 {
        book.apply_snapshot(bids, asks, seq, ts);
        Ok(true)
    } else {
        Ok(book.apply_diff(bids, asks, seq, seq, ts))
    }
}

// ---------- Indodax ----------
// Orderbook channel payloads arrive as {"result":{"channel":"market:orderbook-btcidr","data":{...}}}
// Depth REST shape: {"buy":[[px, qtyIdr?]], "sell":[[..]]} — parse defensively: first two cols.
pub fn parse_indodax_depth(book: &mut OrderBook, raw: &str, ts_ms: i64) -> Result<(), FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let root = if v.get("buy").is_some() || v.get("sell").is_some() {
        &v
    } else {
        &v["result"]["data"]["data"]
    };
    let bids = levels(&root["buy"])?;
    let asks = levels(&root["sell"])?;
    if bids.is_empty() && asks.is_empty() {
        return Err(FeedError::BadPayload {
            venue: Venue::Indodax,
            msg: "empty depth".into(),
        });
    }
    let seq = book.last_update_id + 1;
    book.apply_snapshot(bids, asks, seq, ts_ms);
    Ok(())
}

/// Convert an IDR-quoted book to USD terms with a reference USD/IDR rate.
/// Returns a new book; quantities (base asset) are unchanged, prices divided by rate.
pub fn idr_book_to_usd(book: &OrderBook, usd_idr: Decimal) -> OrderBook {
    let mut out = OrderBook::new(book.venue, Symbol::new(&book.symbol.base, "USDT"));
    if usd_idr <= Decimal::ZERO {
        return out;
    }
    let conv = |l: BookLevel| BookLevel {
        price: l.price / usd_idr,
        qty: l.qty,
    };
    let bids: Vec<_> = book.bids_vec().into_iter().map(conv).collect();
    let asks: Vec<_> = book.asks_vec().into_iter().map(conv).collect();
    out.apply_snapshot(bids, asks, book.last_update_id, book.ts_ms);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn binance_depth_and_ticker() {
        let mut b = OrderBook::new(Venue::Binance, Symbol::new("BTC", "USDT"));
        b.apply_snapshot(vec![], vec![], 0, 0);
        let ok = parse_binance_depth(&mut b,
            r#"{"e":"depthUpdate","E":123,"s":"BTCUSDT","U":1,"u":2,"b":[["100","1"]],"a":[["101","2"]]}"#).unwrap();
        assert!(ok);
        assert_eq!(b.best_bid().unwrap().price, dec!(100));
        let q = parse_binance_book_ticker(
            Symbol::new("BTC", "USDT"),
            r#"{"u":1,"s":"BTCUSDT","b":"100.5","B":"1","a":"101.5","A":"2"}"#,
            99,
        )
        .unwrap();
        assert_eq!(q.bid, dec!(100.5));
    }

    #[test]
    fn bybit_snapshot() {
        let mut b = OrderBook::new(Venue::Bybit, Symbol::new("BTC", "USDT"));
        parse_bybit_orderbook(&mut b,
            r#"{"topic":"orderbook.50.BTCUSDT","type":"snapshot","ts":5,"data":{"s":"BTCUSDT","b":[["99","1"]],"a":[["100","1"]],"u":7}}"#).unwrap();
        assert_eq!(b.best_ask().unwrap().price, dec!(100));
    }

    #[test]
    fn okx_books_snapshot() {
        let mut b = OrderBook::new(Venue::Okx, Symbol::new("BTC", "USDT"));
        parse_okx_books(&mut b,
            r#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"bids":[["99","1","0","1"]],"asks":[["100","1","0","1"]],"ts":"42","seqId":3}]}"#).unwrap();
        assert_eq!(b.best_bid().unwrap().price, dec!(99));
    }

    #[test]
    fn indodax_depth_and_fx() {
        let mut b = OrderBook::new(Venue::Indodax, Symbol::new("BTC", "IDR"));
        parse_indodax_depth(
            &mut b,
            r#"{"buy":[[1600000000,1]],"sell":[[1616000000,1]]}"#,
            10,
        )
        .unwrap();
        let usd = idr_book_to_usd(&b, dec!(16000));
        assert_eq!(usd.best_bid().unwrap().price, dec!(100000));
        assert_eq!(usd.symbol.quote, "USDT");
    }

    #[test]
    fn subscribe_frames_carry_symbols() {
        let s = Symbol::new("BTC", "USDT");
        assert!(subscribe_frame(Venue::Binance, &s)
            .to_string()
            .contains("btcusdt@depth"));
        assert!(subscribe_frame(Venue::Okx, &s)
            .to_string()
            .contains("BTC-USDT"));
        assert!(subscribe_frame(Venue::Bybit, &s)
            .to_string()
            .contains("orderbook.50.BTCUSDT"));
    }
}
