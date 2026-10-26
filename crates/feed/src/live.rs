//! Live venue tasks: one WebSocket loop per CEX venue feeding shared books.
//! Paper mode consumes these through [`pick_fresh`]; stale venues are
//! skipped, never backfilled with synthetic data.

use super::{
    idr_book_to_usd, parse_binance_partial, parse_bybit_orderbook, parse_indodax_depth,
    parse_okx_books, subscribe_frame, ws_url,
};
use arb_core::{now_ms, OrderBook, Symbol, Venue};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared live books, keyed by (venue, `"BASE/QUOTE"`). Indodax books are
/// stored converted to USDT terms (see [`idr_book_to_usd`]).
pub type LiveBooks = Arc<Mutex<BTreeMap<(Venue, String), OrderBook>>>;

/// Freshness gate shared with the engine: a live book is usable when recent
/// and not crossed.
pub fn pick_fresh(
    books: &BTreeMap<(Venue, String), OrderBook>,
    venue: Venue,
    symbol: &str,
    now: i64,
    max_age_ms: i64,
) -> Option<OrderBook> {
    let b = books.get(&(venue, symbol.to_string()))?;
    if now - b.ts_ms <= max_age_ms && !b.is_crossed() {
        Some(b.clone())
    } else {
        None
    }
}

fn lock_map(shared: &LiveBooks) -> std::sync::MutexGuard<'_, BTreeMap<(Venue, String), OrderBook>> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawn one reconnecting WS task per CEX venue. Returns immediately; tasks
/// publish into `shared` until the runtime shuts down.
pub fn spawn_venue_tasks(
    shared: &LiveBooks,
    venues: Vec<Venue>,
    symbols: Vec<Symbol>,
    usd_idr: Decimal,
) {
    // Pin the TLS crypto provider explicitly: feature unification across the
    // tree can leave rustls with an ambiguous provider set, which panics on
    // first use. Explicit install always wins over auto-detection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    for v in venues {
        if !v.is_cex() || ws_url(v).is_empty() {
            continue;
        }
        let (shared, symbols) = (shared.clone(), symbols.clone());
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                if let Err(e) = connect_and_stream(v, &symbols, usd_idr, &shared).await {
                    tracing::warn!(venue = %v, %e, backoff_s = backoff.as_secs(), "live feed down; reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                } else {
                    backoff = Duration::from_secs(1);
                }
            }
        });
    }
}

/// Raw subscription symbols: Indodax quotes in IDR, everything else as-is.
fn raw_symbols(venue: Venue, symbols: &[Symbol]) -> Vec<Symbol> {
    if venue == Venue::Indodax {
        symbols
            .iter()
            .map(|s| Symbol::new(&s.base, "IDR"))
            .collect()
    } else {
        symbols.to_vec()
    }
}

fn venue_spelling(venue: Venue, s: &Symbol) -> String {
    match venue {
        Venue::Binance => s.binance(),
        Venue::Bybit => s.bybit(),
        Venue::Okx => s.okx(),
        Venue::Indodax => s.indodax(),
        _ => s.to_string(),
    }
}

async fn connect_and_stream(
    venue: Venue,
    symbols: &[Symbol],
    usd_idr: Decimal,
    shared: &LiveBooks,
) -> anyhow::Result<()> {
    let raw = raw_symbols(venue, symbols);
    // Binance partial-book payloads carry no symbol, so use one combined
    // stream URL (payloads arrive wrapped with their stream name).
    let url = if venue == Venue::Binance {
        let streams: Vec<String> = raw
            .iter()
            .map(|s| format!("{}@depth20@100ms", venue_spelling(venue, s)))
            .collect();
        format!("{}/stream?streams={}", ws_url(venue), streams.join("/"))
    } else {
        ws_url(venue).to_string()
    };
    tracing::info!(venue = %venue, "dialing live feed");
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(&url),
    )
    .await??;
    let (mut sink, mut stream) = ws.split();
    if venue != Venue::Binance {
        use futures_util::SinkExt;
        for s in &raw {
            let frame = subscribe_frame(venue, s).to_string();
            sink.send(tokio_tungstenite::tungstenite::Message::Text(frame.into()))
                .await?;
        }
    }
    tracing::info!(venue = %venue, books = raw.len(), "live feed connected");
    let mut books: BTreeMap<String, OrderBook> = BTreeMap::new();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(30), stream.next()).await?;
        let Some(msg) = msg else {
            anyhow::bail!("stream closed by venue");
        };
        let msg = msg?;
        if !msg.is_text() {
            continue;
        }
        if let Some(key) = apply_msg(venue, &raw, &mut books, msg.to_text()?, now_ms()) {
            let mut map = lock_map(shared);
            if venue == Venue::Indodax {
                if let Some(raw_book) = books.get(&key) {
                    let base = key.split('/').next().unwrap_or(&key);
                    map.insert(
                        (venue, format!("{base}/USDT")),
                        idr_book_to_usd(raw_book, usd_idr),
                    );
                }
            } else if let Some(b) = books.get(&key) {
                map.insert((venue, key), b.clone());
            }
        }
    }
}
/// Route one text message into its book. Returns the raw book key touched
/// (`"BASE/QUOTE"`, IDR-quoted on Indodax), or `None` when the message
/// carries no book (subscription acks, tickers, other channels).
fn apply_msg(
    venue: Venue,
    raw: &[Symbol],
    books: &mut BTreeMap<String, OrderBook>,
    msg: &str,
    now: i64,
) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(msg).ok()?;
    let spelling: String = match venue {
        Venue::Binance => {
            // Combined-stream wrapper: {"stream":"btcusdt@depth20@100ms","data":{...}}.
            let stream = v.get("stream")?.as_str()?;
            let inner = &v["data"];
            inner.get("bids").and_then(|b| b.as_array())?;
            let sym = raw
                .iter()
                .find(|s| stream.starts_with(&venue_spelling(venue, s)))?;
            let key = sym.to_string();
            let book = books
                .entry(key.clone())
                .or_insert_with(|| OrderBook::new(venue, sym.clone()));
            parse_binance_partial(book, &inner.to_string(), now).ok()?;
            return Some(key);
        }
        Venue::Bybit => {
            let topic = v.get("topic")?.as_str()?;
            let tag = topic.strip_prefix("orderbook.")?;
            let sym = raw.iter().find(|s| tag.ends_with(&s.bybit()))?;
            parse_bybit_orderbook(
                books
                    .entry(sym.to_string())
                    .or_insert_with(|| OrderBook::new(venue, sym.clone())),
                msg,
            )
            .ok()?;
            sym.to_string()
        }
        Venue::Okx => {
            if v.get("arg")?.get("channel")?.as_str()? != "books" {
                return None;
            }
            let inst = v.get("arg")?.get("instId")?.as_str()?;
            let sym = raw.iter().find(|s| s.okx() == inst)?;
            parse_okx_books(
                books
                    .entry(sym.to_string())
                    .or_insert_with(|| OrderBook::new(venue, sym.clone())),
                msg,
            )
            .ok()?;
            sym.to_string()
        }
        Venue::Indodax => {
            let channel = v.get("result")?.get("channel")?.as_str()?;
            let tag = channel.strip_prefix("market:orderbook-")?;
            let sym = raw.iter().find(|s| s.indodax() == tag)?;
            parse_indodax_depth(
                books
                    .entry(sym.to_string())
                    .or_insert_with(|| OrderBook::new(venue, sym.clone())),
                msg,
                now,
            )
            .ok()?;
            sym.to_string()
        }
        _ => return None,
    };
    Some(spelling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{BookLevel, Side};
    use rust_decimal_macros::dec;

    fn book(venue: Venue, sym: &str, ts: i64) -> OrderBook {
        let (b, q) = sym.split_once('/').unwrap();
        let mut o = OrderBook::new(venue, Symbol::new(b, q));
        o.apply_snapshot(
            vec![BookLevel {
                price: dec!(99),
                qty: dec!(1),
            }],
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(1),
            }],
            1,
            ts,
        );
        o
    }

    #[test]
    fn fresh_gate_accepts_recent_and_rejects_stale() {
        let map = BTreeMap::from([
            (
                (Venue::Binance, "BTC/USDT".into()),
                book(Venue::Binance, "BTC/USDT", 900),
            ),
            (
                (Venue::Bybit, "BTC/USDT".into()),
                book(Venue::Bybit, "BTC/USDT", 100),
            ),
        ]);
        assert!(pick_fresh(&map, Venue::Binance, "BTC/USDT", 1000, 2000).is_some());
        assert!(pick_fresh(&map, Venue::Bybit, "BTC/USDT", 1000, 200).is_none());
        assert!(pick_fresh(&map, Venue::Okx, "BTC/USDT", 1000, 2000).is_none());
    }

    #[test]
    fn crossed_live_book_is_skipped() {
        let mut o = book(Venue::Binance, "BTC/USDT", 900);
        o.apply_snapshot(
            vec![BookLevel {
                price: dec!(101),
                qty: dec!(1),
            }],
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(1),
            }],
            2,
            950,
        );
        let map = BTreeMap::from([((Venue::Binance, "BTC/USDT".into()), o)]);
        assert!(pick_fresh(&map, Venue::Binance, "BTC/USDT", 1000, 2000).is_none());
    }

    #[test]
    fn router_feeds_each_venue_book() {
        let syms = vec![Symbol::new("BTC", "USDT")];
        let mut books = BTreeMap::new();
        let hit = apply_msg(
            Venue::Bybit,
            &syms,
            &mut books,
            r#"{"topic":"orderbook.50.BTCUSDT","type":"snapshot","ts":5,"data":{"s":"BTCUSDT","b":[["99","1"]],"a":[["100","1"]],"u":7}}"#,
            5,
        );
        assert_eq!(hit.as_deref(), Some("BTC/USDT"));
        let b = books["BTC/USDT"].vwap(Side::Buy, dec!(1)).unwrap();
        assert_eq!(b, dec!(100));
        // Ticker / ack traffic touches nothing.
        let miss = apply_msg(
            Venue::Bybit,
            &syms,
            &mut books,
            r#"{"topic":"tickers.BTCUSDT","data":{}}"#,
            6,
        );
        assert!(miss.is_none());
    }

    #[test]
    fn router_converts_indodax_to_usdt_key() {
        let raws = raw_symbols(Venue::Indodax, &[Symbol::new("BTC", "USDT")]);
        assert_eq!(raws[0], Symbol::new("BTC", "IDR"));
        let mut books = BTreeMap::new();
        let hit = apply_msg(
            Venue::Indodax,
            &raws,
            &mut books,
            r#"{"result":{"channel":"market:orderbook-btcidr","data":{"data":{"buy":[[1600000000,1]],"sell":[[1616000000,1]]}}}}"#,
            10,
        );
        assert_eq!(hit.as_deref(), Some("BTC/IDR"));
    }
}
