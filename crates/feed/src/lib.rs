//! Market-data connectors. Parsers turn venue wire formats into `OrderBook`/`Quote`.
//! Live WS loops plug into these parsers; URLs and subscribe frames live here so
//! they are testable without a network.

use arb_core::{BookLevel, OrderBook, Quote, Symbol, Venue};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

pub mod dex;
pub mod funding;
pub mod fx;
pub mod live;
pub use dex::{spawn_dex_tasks, DexKind, EvmCfg, JupiterCfg, LiveDexQuotes};
pub use funding::{spawn_funding_tasks, LiveFunding};
pub use fx::{
    implied_usdt_idr, resolve_usdt_idr, FxCrossCheck, FxSample, RateDecision, RateState,
    UsdtIdrRate,
};
pub use live::{
    pick_fresh, spawn_upbit_usdt_idr_task, spawn_venue_tasks, ConnectorState, LiveBooks,
    LiveConnectors,
};

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("bad payload for {venue}: {msg}")]
    BadPayload { venue: Venue, msg: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) fn dec(venue: Venue, s: &str) -> Result<Decimal, FeedError> {
    Decimal::from_str(s).map_err(|e| FeedError::BadPayload {
        venue,
        msg: e.to_string(),
    })
}

pub(crate) fn dec_val(venue: Venue, v: &Value) -> Result<Decimal, FeedError> {
    match v {
        Value::String(s) => dec(venue, s),
        Value::Number(n) => Decimal::from_str(&n.to_string()).map_err(|e| FeedError::BadPayload {
            venue,
            msg: e.to_string(),
        }),
        _ => Err(FeedError::BadPayload {
            venue,
            msg: format!("not numeric: {v}"),
        }),
    }
}

fn levels(venue: Venue, arr: &Value) -> Result<Vec<BookLevel>, FeedError> {
    let mut out = Vec::new();
    for row in arr.as_array().cloned().unwrap_or_default() {
        let r = row.as_array().cloned().unwrap_or_default();
        if r.len() >= 2 {
            out.push(BookLevel {
                price: dec_val(venue, &r[0])?,
                qty: dec_val(venue, &r[1])?,
            });
        }
    }
    Ok(out)
}

/// Public endpoints (verified against official docs; see README sources).
/// `BINANCE_WS_URL` overrides the Binance base (e.g. the spot testnet
/// `wss://testnet.binance.vision`); `BYBIT_WS_URL` overrides the Bybit spot
/// base (e.g. `wss://stream-testnet.bybit.com/v5/public/spot`);
/// `KRAKEN_WS_URL` overrides the Kraken v2 base (`wss://ws.kraken.com/v2`);
/// empty/unset keeps mainnet.
pub fn ws_url(venue: Venue) -> String {
    if venue == Venue::Binance {
        if let Ok(u) = std::env::var("BINANCE_WS_URL") {
            if !u.trim().is_empty() {
                return u;
            }
        }
    }
    if venue == Venue::Bybit {
        if let Ok(u) = std::env::var("BYBIT_WS_URL") {
            if !u.trim().is_empty() {
                return u;
            }
        }
    }
    if venue == Venue::Kraken {
        if let Ok(u) = std::env::var("KRAKEN_WS_URL") {
            if !u.trim().is_empty() {
                return u;
            }
        }
    }
    match venue {
        Venue::Binance => "wss://stream.binance.com:9443",
        Venue::Bybit => "wss://stream.bybit.com/v5/public/spot",
        Venue::Okx => "wss://ws.okx.com:8443/ws/v5/public",
        Venue::Indodax => "wss://ws3.indodax.com/ws/",
        Venue::Kraken => "wss://ws.kraken.com/v2",
        Venue::Uniswap | Venue::Pancakeswap | Venue::Jupiter => "",
    }
    .to_string()
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
            "params": [format!("{}@depth20@100ms", symbol.binance()), format!("{}@bookTicker", symbol.binance())],
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
            "params": {"channel": format!("market:order-book-{}", symbol.indodax())},
            "id": 2
        }),
        // Kraken v2: one `book` subscription covers the symbol (depth 10,
        // snapshot first, then deltas). Verified live 2026-10-29.
        Venue::Kraken => serde_json::json!({
            "method": "subscribe",
            "params": {"channel": "book", "symbol": [symbol.kraken()], "depth": 10, "snapshot": true}
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
    Ok(book.apply_diff(
        levels(Venue::Binance, &v["b"])?,
        levels(Venue::Binance, &v["a"])?,
        first,
        fin,
        ts,
    ))
}

// Partial book (depth20@100ms): {"lastUpdateId":1,"bids":[[px,qty]],"asks":[[..]]}.
// Full top-20 image on every push, so no REST snapshot sync is needed.
pub fn parse_binance_partial(
    book: &mut OrderBook,
    raw: &str,
    ts_ms: i64,
) -> Result<bool, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let seq = v["lastUpdateId"].as_u64().unwrap_or(0);
    book.apply_snapshot(
        levels(Venue::Binance, &v["bids"])?,
        levels(Venue::Binance, &v["asks"])?,
        seq,
        ts_ms,
    );
    Ok(true)
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
        bid: dec_val(Venue::Binance, &v["b"])?,
        ask: dec_val(Venue::Binance, &v["a"])?,
        bid_qty: dec_val(Venue::Binance, &v["B"])?,
        ask_qty: dec_val(Venue::Binance, &v["A"])?,
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
    let (bids, asks) = (
        levels(Venue::Bybit, &d["b"])?,
        levels(Venue::Bybit, &d["a"])?,
    );
    if v["type"].as_str() == Some("snapshot") {
        book.apply_snapshot(bids, asks, seq, ts);
        Ok(true)
    } else {
        // Bybit deltas carry no first-id; treat sequence as both bounds.
        // An empty book has no snapshot yet: never seed it from a delta.
        if book.last_update_id == 0 {
            return Ok(false);
        }
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
    // OKX checkpoints with prevSeqId; map it onto the Binance-style
    // (first, final) contract. Passing seqId as both ends freezes the
    // book forever after a single missed update.
    let first = d["prevSeqId"]
        .as_u64()
        .map(|p| p.saturating_add(1))
        .unwrap_or(seq);
    let (bids, asks) = (
        levels(Venue::Okx, &d["bids"])?,
        levels(Venue::Okx, &d["asks"])?,
    );
    if v["action"].as_str() == Some("snapshot") || book.last_update_id == 0 {
        book.apply_snapshot(bids, asks, seq, ts);
        Ok(true)
    } else {
        Ok(book.apply_diff(bids, asks, first, seq, ts))
    }
}

// ---------- Indodax ----------
// Live orderbook payloads arrive as
// {"result":{"channel":"market:order-book-btcidr","data":{"data":
//   {"pair":"btcidr","bid":[{"price":"..","btc_volume":"..",..}],"ask":[..]}}}}
// The depth REST shape is {"buy":[[px, qty]], "sell":[[..]]}; both are parsed.
fn indodax_object_levels(arr: &Value, base: &str) -> Result<Vec<BookLevel>, FeedError> {
    let vol_key = format!("{}_volume", base.to_lowercase());
    let mut out = Vec::new();
    for row in arr.as_array().cloned().unwrap_or_default() {
        out.push(BookLevel {
            price: dec_val(Venue::Indodax, &row["price"])?,
            qty: dec_val(Venue::Indodax, &row[&vol_key])?,
        });
    }
    Ok(out)
}

pub fn parse_indodax_depth(book: &mut OrderBook, raw: &str, ts_ms: i64) -> Result<(), FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let root = if v.get("buy").is_some()
        || v.get("sell").is_some()
        || v.get("bid").is_some()
        || v.get("ask").is_some()
    {
        &v
    } else {
        &v["result"]["data"]["data"]
    };
    let (bids, asks) = if root.get("bid").is_some() || root.get("ask").is_some() {
        (
            indodax_object_levels(&root["bid"], &book.symbol.base)?,
            indodax_object_levels(&root["ask"], &book.symbol.base)?,
        )
    } else {
        (
            levels(Venue::Indodax, &root["buy"])?,
            levels(Venue::Indodax, &root["sell"])?,
        )
    };
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

// ---------- Kraken v2 ----------
// Book: {"channel":"book","type":"snapshot|update","data":[{"symbol":"BTC/USDT",
// "bids":[{"price":79911.7,"qty":0.01}],"asks":[...],"checksum":n}]}.
// Updates carry only changed levels; `qty: 0` removes the level (handled by
// `apply_diff`'s upsert). Levels are numeric objects on WS, strings on REST;
// both parse via `dec_val`. No per-message sequence id, so updates map onto
// the diff contract as the next local id. `checksum` is not verified;
// recovery is reconnect + `snapshot: true` on resubscribe (fresh books map,
// so a dropped connection never resumes from stale state).
pub fn parse_kraken_book(book: &mut OrderBook, raw: &str, ts_ms: i64) -> Result<bool, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let d = &v["data"][0];
    let (bids, asks) = (kraken_levels(&d["bids"])?, kraken_levels(&d["asks"])?);
    if bids.is_empty() && asks.is_empty() {
        return Err(FeedError::BadPayload {
            venue: Venue::Kraken,
            msg: "empty book".into(),
        });
    }
    if v["type"].as_str() == Some("snapshot") {
        let seq = book.last_update_id + 1;
        book.apply_snapshot(bids, asks, seq, ts_ms);
        Ok(true)
    } else if book.last_update_id == 0 {
        // No snapshot yet: never seed the book from a delta.
        Ok(false)
    } else {
        let seq = book.last_update_id + 1;
        Ok(book.apply_diff(bids, asks, seq, seq, ts_ms))
    }
}

fn kraken_levels(arr: &Value) -> Result<Vec<BookLevel>, FeedError> {
    let mut out = Vec::new();
    for row in arr.as_array().cloned().unwrap_or_default() {
        out.push(BookLevel {
            price: dec_val(Venue::Kraken, &row["price"])?,
            qty: dec_val(Venue::Kraken, &row["qty"])?,
        });
    }
    Ok(out)
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
    fn bybit_delta_needs_snapshot_first() {
        let mut b = OrderBook::new(Venue::Bybit, Symbol::new("BTC", "USDT"));
        let ok = parse_bybit_orderbook(&mut b,
            r#"{"topic":"orderbook.50.BTCUSDT","type":"delta","ts":5,"data":{"s":"BTCUSDT","b":[["99","1"]],"a":[["100","1"]],"u":7}}"#).unwrap();
        assert!(!ok);
        assert_eq!(b.last_update_id, 0);
    }

    #[test]
    fn okx_books_snapshot() {
        let mut b = OrderBook::new(Venue::Okx, Symbol::new("BTC", "USDT"));
        parse_okx_books(&mut b,
            r#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"bids":[["99","1","0","1"]],"asks":[["100","1","0","1"]],"ts":"42","seqId":3}]}"#).unwrap();
        assert_eq!(b.best_bid().unwrap().price, dec!(99));
    }

    #[test]
    fn okx_chained_update_accepts_and_gap_rejects() {
        // Regression: mapping seqId onto both ends of the diff contract
        // froze every OKX book after one missed update (0/20 fresh).
        let snap = r#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"bids":[["99","1","0","1"]],"asks":[["100","1","0","1"]],"ts":"42","seqId":10,"prevSeqId":9}]}"#;
        let next = r#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"bids":[["98","1","0","1"]],"asks":[["100","1","0","1"]],"ts":"43","seqId":11,"prevSeqId":10}]}"#;
        let gapped = r#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"bids":[["97","1","0","1"]],"asks":[["100","1","0","1"]],"ts":"44","seqId":14,"prevSeqId":12}]}"#;
        let mut b = OrderBook::new(Venue::Okx, Symbol::new("BTC", "USDT"));
        assert!(parse_okx_books(&mut b, snap).unwrap());
        assert!(parse_okx_books(&mut b, next).unwrap());
        assert_eq!(b.last_update_id, 11);
        assert!(!parse_okx_books(&mut b, gapped).unwrap());
        assert_eq!(b.last_update_id, 11);
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
    fn indodax_live_object_depth() {
        // Regression: the live WS payload is bid/ask objects keyed by
        // `<base>_volume`, not the REST buy/sell arrays. The old parser
        // read it as empty depth, so no Indodax book ever landed.
        let mut b = OrderBook::new(Venue::Indodax, Symbol::new("BTC", "IDR"));
        parse_indodax_depth(
            &mut b,
            r#"{"result":{"channel":"market:order-book-btcidr","data":{"data":{"pair":"btcidr","ask":[{"price":"1445022000","btc_volume":"0.54152019","idr_volume":"782508588"}],"bid":[{"price":"1445021000","btc_volume":"0.25","idr_volume":"361255250"}]},"offset":74487609}}}"#,
            10,
        )
        .unwrap();
        assert_eq!(b.best_bid().unwrap().price, dec!(1445021000));
        assert_eq!(b.best_bid().unwrap().qty, dec!(0.25));
        assert_eq!(b.best_ask().unwrap().price, dec!(1445022000));
        assert_eq!(b.best_ask().unwrap().qty, dec!(0.54152019));
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
        // Indodax documents `market:order-book-<pair>` (hyphenated), and
        // the connection must be authenticated before subscribing.
        assert!(subscribe_frame(Venue::Indodax, &Symbol::new("BTC", "IDR"))
            .to_string()
            .contains("market:order-book-btcidr"));
        // Kraken v2 subscribes `book` with slash-separated symbols. This
        // must never be Null: the wildcard would compile and idle the venue.
        let kraken = subscribe_frame(Venue::Kraken, &s);
        assert!(
            kraken.to_string().contains("\"book\"") && kraken.to_string().contains("BTC/USDT"),
            "{kraken}"
        );
        let auth = indodax_auth_frame("tok123").to_string();
        assert!(
            auth.contains("tok123") && auth.contains("\"id\":1"),
            "{auth}"
        );
    }

    #[test]
    fn ws_endpoints_are_pinned() {
        // A wrong URL fails silently at connect time (reconnect loop with
        // no books), so pin the exact endpoints the tasks dial.
        assert_eq!(ws_url(Venue::Binance), "wss://stream.binance.com:9443");
        assert_eq!(
            ws_url(Venue::Bybit),
            "wss://stream.bybit.com/v5/public/spot"
        );
        assert_eq!(ws_url(Venue::Okx), "wss://ws.okx.com:8443/ws/v5/public");
        assert_eq!(ws_url(Venue::Indodax), "wss://ws3.indodax.com/ws/");
        assert_eq!(ws_url(Venue::Kraken), "wss://ws.kraken.com/v2");
        assert_eq!(ws_url(Venue::Uniswap), "");
        assert_eq!(perp_ws_url(Venue::Binance), "wss://fstream.binance.com");
        assert_eq!(
            perp_ws_url(Venue::Bybit),
            "wss://stream.bybit.com/v5/public/linear"
        );
    }

    #[test]
    fn kraken_book_snapshot_then_delta() {
        // Live v2 shapes (verified 2026-10-29): numeric price/qty objects,
        // `qty: 0` removes the level, deltas never seed an empty book.
        let mut b = OrderBook::new(Venue::Kraken, Symbol::new("BTC", "USDT"));
        assert!(!parse_kraken_book(
            &mut b,
            r#"{"channel":"book","type":"update","data":[{"symbol":"BTC/USDT","bids":[],"asks":[{"price":80019.6,"qty":0.0126}],"checksum":1}]}"#,
            5,
        )
        .unwrap());
        assert!(parse_kraken_book(
            &mut b,
            r#"{"channel":"book","type":"snapshot","data":[{"symbol":"BTC/USDT","bids":[{"price":79782.2,"qty":0.127}],"asks":[{"price":79782.3,"qty":0.19}],"checksum":2}]}"#,
            6,
        )
        .unwrap());
        assert_eq!(b.best_bid().unwrap().price, dec!(79782.2));
        assert!(parse_kraken_book(
            &mut b,
            r#"{"channel":"book","type":"update","data":[{"symbol":"BTC/USDT","bids":[{"price":79782.2,"qty":0}],"asks":[],"checksum":3}]}"#,
            7,
        )
        .unwrap());
        assert!(b.best_bid().is_none());
        assert_eq!(b.best_ask().unwrap().price, dec!(79782.3));
    }

    #[test]
    fn binance_partial_book_snapshots() {
        let mut b = OrderBook::new(Venue::Binance, Symbol::new("BTC", "USDT"));
        parse_binance_partial(
            &mut b,
            r#"{"lastUpdateId":9,"bids":[["99","1"]],"asks":[["100","2"]]}"#,
            7,
        )
        .unwrap();
        assert_eq!(b.best_bid().unwrap().price, dec!(99));
        assert_eq!(b.best_ask().unwrap().price, dec!(100));
        assert_eq!(b.ts_ms, 7);
    }
}
