//! Live venue tasks: one WebSocket loop per CEX venue feeding shared books.
//! Paper mode consumes these through [`pick_fresh`]; stale venues are
//! skipped, never backfilled with synthetic data.

use super::fx::{parse_upbit_orderbook, upbit_subscribe_frame, FxCrossCheck, UPBIT_ID_WS_URL};
use super::{
    indodax_auth_frame, parse_binance_partial, parse_bybit_orderbook, parse_indodax_depth,
    parse_okx_books, subscribe_frame, ws_url,
};
use arb_core::{now_ms, OrderBook, Symbol, Venue};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared live books, keyed by (venue, `"BASE/QUOTE"`). Indodax books are
/// stored raw in IDR terms (including the `USDT/IDR` rate book); the
/// engine converts per tick with the selected live rate (see [`super::fx`]).
pub type LiveBooks = Arc<Mutex<BTreeMap<(Venue, String), OrderBook>>>;

/// Socket-level state for one venue task, tracked separately from book
/// freshness: a connected socket whose books are momentarily quiet is a
/// different diagnosis from a socket that is down.
#[derive(Debug, Clone, Default)]
pub struct ConnectorState {
    pub connected: bool,
    /// Last WebSocket frame of any kind received from the venue.
    pub last_msg_ms: Option<i64>,
    pub last_connected_ms: Option<i64>,
    pub last_error: Option<String>,
}

/// Shared per-venue connection state, written by the venue tasks.
pub type LiveConnectors = Arc<Mutex<BTreeMap<Venue, ConnectorState>>>;

fn update_connector(
    connectors: &LiveConnectors,
    venue: Venue,
    f: impl FnOnce(&mut ConnectorState),
) {
    let mut map = connectors.lock().unwrap_or_else(|e| e.into_inner());
    f(map.entry(venue).or_default());
}

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
/// publish into `shared` until the runtime shuts down. Indodax requires
/// its published static market-data token (`indodax_token`, from the
/// environment) before any subscription; without it that venue is skipped
/// with one clear error instead of a reconnect loop.
pub fn spawn_venue_tasks(
    shared: &LiveBooks,
    connectors: &LiveConnectors,
    venues: Vec<Venue>,
    symbols: Vec<Symbol>,
    indodax_token: Option<String>,
) {
    // Pin the TLS crypto provider explicitly: feature unification across the
    // tree can leave rustls with an ambiguous provider set, which panics on
    // first use. Explicit install always wins over auto-detection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    for v in venues {
        if !v.is_cex() || ws_url(v).is_empty() {
            continue;
        }
        if v == Venue::Indodax && indodax_token.is_none() {
            let msg = "INDODAX_WS_TOKEN not set; Indodax feed disabled (its WS rejects unauthenticated subscriptions)".to_string();
            tracing::error!(venue = %v, "{msg}");
            update_connector(connectors, v, |c| c.last_error = Some(msg));
            continue;
        }
        let (shared, connectors, symbols) = (shared.clone(), connectors.clone(), symbols.clone());
        let token = indodax_token.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                update_connector(&connectors, v, |c| c.connected = false);
                match connect_and_stream(v, &symbols, &shared, &connectors, token.as_deref()).await
                {
                    Err(e) => {
                        update_connector(&connectors, v, |c| {
                            c.connected = false;
                            c.last_error = Some(e.to_string());
                        });
                        tracing::warn!(venue = %v, %e, backoff_s = backoff.as_secs(), "live feed down; reconnecting");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                    Ok(()) => backoff = Duration::from_secs(1),
                }
            }
        });
    }
}

/// Raw subscription symbols: Indodax quotes in IDR, everything else as-is.
/// Indodax additionally always carries `USDT/IDR` itself: that book is the
/// primary live rate the engine converts the other IDR books with.
fn raw_symbols(venue: Venue, symbols: &[Symbol]) -> Vec<Symbol> {
    if venue == Venue::Indodax {
        let mut out: Vec<Symbol> = symbols
            .iter()
            .map(|s| Symbol::new(&s.base, "IDR"))
            .collect();
        let rate = Symbol::new("USDT", "IDR");
        if !out.contains(&rate) {
            out.push(rate);
        }
        out
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
    shared: &LiveBooks,
    connectors: &LiveConnectors,
    indodax_token: Option<&str>,
) -> anyhow::Result<()> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
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
    update_connector(connectors, venue, |c| {
        c.connected = true;
        c.last_connected_ms = Some(now_ms());
        c.last_error = None;
    });
    if venue == Venue::Indodax {
        // Indodax closes unauthenticated subscriptions as "bad request"
        // (verified against the live endpoint). Authenticate with the
        // published static token first; commands on one socket are
        // processed in order, so subscriptions can follow immediately.
        let token = indodax_token.ok_or_else(|| anyhow::anyhow!("INDODAX_WS_TOKEN not set"))?;
        sink.send(Message::Text(indodax_auth_frame(token).to_string().into()))
            .await?;
    }
    if venue != Venue::Binance {
        for (i, s) in raw.iter().enumerate() {
            let mut frame = subscribe_frame(venue, s);
            if venue == Venue::Indodax {
                // Request ids must uniquely identify each request; the
                // auth frame already used id 1.
                frame["id"] = serde_json::json!(2 + i);
            }
            sink.send(Message::Text(frame.to_string().into())).await?;
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
        update_connector(connectors, venue, |c| {
            c.connected = true;
            c.last_msg_ms = Some(now_ms());
        });
        match msg {
            Message::Ping(p) => {
                sink.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Close(f) => {
                anyhow::bail!("stream closed by venue: {f:?}");
            }
            _ if !msg.is_text() => continue,
            _ => {}
        }
        if let Some(key) = apply_msg(venue, &raw, &mut books, msg.to_text()?, now_ms()) {
            // Books publish raw in their quoted terms. IDR conversion moved
            // to the engine tick, where one snapshot yields one live rate.
            let mut map = lock_map(shared);
            if let Some(b) = books.get(&key) {
                map.insert((venue, key), b.clone());
            }
        }
    }
}
/// Spawn the independent USDT/IDR cross-check stream (Upbit Indonesia,
/// public, no key). This feeds rate validation only; it is not a trading
/// venue and never becomes the conversion divisor.
pub fn spawn_upbit_usdt_idr_task(cross: &FxCrossCheck) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cross = cross.clone();
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match stream_upbit_usdt_idr(&cross).await {
                Err(e) => {
                    tracing::warn!(%e, backoff_s = backoff.as_secs(), "upbit IDR cross-check down; reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                Ok(()) => backoff = Duration::from_secs(1),
            }
        }
    });
}

async fn stream_upbit_usdt_idr(cross: &FxCrossCheck) -> anyhow::Result<()> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(UPBIT_ID_WS_URL),
    )
    .await??;
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::Text(upbit_subscribe_frame().to_string().into()))
        .await?;
    tracing::info!("upbit USDT/IDR cross-check connected");
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(30), stream.next()).await?;
        let Some(msg) = msg else {
            anyhow::bail!("stream closed by venue");
        };
        match msg? {
            Message::Ping(p) => sink.send(Message::Pong(p)).await?,
            Message::Close(f) => anyhow::bail!("stream closed by venue: {f:?}"),
            Message::Text(t) => {
                if let Some(sample) = parse_upbit_orderbook(&t) {
                    *cross.lock().unwrap_or_else(|e| e.into_inner()) = Some(sample);
                }
            }
            Message::Binary(b) => {
                if let Ok(t) = std::str::from_utf8(&b) {
                    if let Some(sample) = parse_upbit_orderbook(t) {
                        *cross.lock().unwrap_or_else(|e| e.into_inner()) = Some(sample);
                    }
                }
            }
            _ => {}
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
            let tag = channel
                .strip_prefix("market:order-book-")
                .or_else(|| channel.strip_prefix("market:orderbook-"))?;
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
    fn router_keeps_indodax_books_raw_and_carries_the_rate_pair() {
        let raws = raw_symbols(Venue::Indodax, &[Symbol::new("BTC", "USDT")]);
        assert_eq!(raws[0], Symbol::new("BTC", "IDR"));
        // The USDT/IDR rate book rides along exactly once, even when the
        // configured symbols already imply it.
        assert_eq!(
            raws.iter()
                .filter(|s| **s == Symbol::new("USDT", "IDR"))
                .count(),
            1
        );
        let dupes = raw_symbols(
            Venue::Indodax,
            &[Symbol::new("BTC", "USDT"), Symbol::new("USDT", "USDT")],
        );
        assert_eq!(
            dupes
                .iter()
                .filter(|s| **s == Symbol::new("USDT", "IDR"))
                .count(),
            1
        );
        let mut books = BTreeMap::new();
        // Current live wire shape (documented channel + bid/ask objects).
        let hit = apply_msg(
            Venue::Indodax,
            &raws,
            &mut books,
            r#"{"result":{"channel":"market:order-book-btcidr","data":{"data":{"pair":"btcidr","ask":[{"price":"1616000000","btc_volume":"1"}],"bid":[{"price":"1600000000","btc_volume":"1"}]}}}}"#,
            10,
        );
        assert_eq!(hit.as_deref(), Some("BTC/IDR"));
        assert_eq!(books["BTC/IDR"].best_ask().unwrap().price, dec!(1616000000));
        // The rate pair parses under its own key — never as USDT/USDT.
        let hit = apply_msg(
            Venue::Indodax,
            &raws,
            &mut books,
            r#"{"result":{"channel":"market:order-book-usdtidr","data":{"data":{"pair":"usdtidr","ask":[{"price":"18112","usdt_volume":"5000"}],"bid":[{"price":"18111","usdt_volume":"7000"}]}}}}"#,
            11,
        );
        assert_eq!(hit.as_deref(), Some("USDT/IDR"));
        assert_eq!(books["USDT/IDR"].best_bid().unwrap().price, dec!(18111));
        assert_eq!(books["USDT/IDR"].best_ask().unwrap().qty, dec!(5000));
        assert!(!books.contains_key("USDT/USDT"));
        // Auth and subscribe acknowledgements touch no book.
        let ack = apply_msg(
            Venue::Indodax,
            &raws,
            &mut books,
            r#"{"id":1,"result":{"client":"abc","version":"2.8.7"}}"#,
            11,
        );
        assert!(ack.is_none());
    }

    #[test]
    fn connector_state_records_connects_and_errors() {
        // Every venue task reports through here; a broken write would
        // blind the /status connector health the engine serves.
        let connectors: LiveConnectors = Arc::new(Mutex::new(BTreeMap::new()));
        update_connector(&connectors, Venue::Binance, |c| {
            c.connected = true;
            c.last_msg_ms = Some(42);
        });
        update_connector(&connectors, Venue::Binance, |c| {
            c.connected = false;
            c.last_error = Some("boom".into());
        });
        let map = connectors.lock().unwrap();
        let c = &map[&Venue::Binance];
        assert!(!c.connected);
        assert_eq!(c.last_msg_ms, Some(42));
        assert_eq!(c.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn raw_symbols_pass_through_non_idr_venues() {
        let syms = vec![Symbol::new("BTC", "USDT")];
        assert_eq!(raw_symbols(Venue::Binance, &syms), syms);
    }
}
