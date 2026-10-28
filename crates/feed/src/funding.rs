//! Live perp funding feeds (read-only WebSockets, no orders).
//!
//! One reconnecting task per perp venue pushes `FundingRate` prints into a
//! shared map stamped at receipt. The engine keeps only prints fresher than
//! `max_book_age_ms` — a stalled funding stream degrades to no entries,
//! never to stale entries. Messages without funding fields (deltas,
//! heartbeats, subscribe acks) parse to `None` and are routine, not errors.

use crate::{perp_ws_url, FeedError};
use arb_core::{now_ms, Symbol, Venue};
use arb_strategy::FundingRate;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Latest funding prints keyed by (venue, `"BASE/QUOTE"`), with receipt time.
pub type LiveFunding = Arc<Mutex<BTreeMap<(Venue, String), (FundingRate, i64)>>>;

fn num(v: &Value) -> Result<Decimal, FeedError> {
    match v {
        Value::String(s) => Decimal::from_str(s).map_err(|e| FeedError::BadPayload {
            venue: Venue::Binance,
            msg: e.to_string(),
        }),
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

/// Perp instrument spelling per venue: Binance `btcusdt`, Bybit `BTCUSDT`,
/// OKX `BTC-USDT-SWAP`.
fn perp_spelling(venue: Venue, s: &Symbol) -> String {
    match venue {
        Venue::Binance => format!("{}{}", s.base, s.quote).to_lowercase(),
        Venue::Bybit => format!("{}{}", s.base, s.quote),
        Venue::Okx => format!("{}-{}-SWAP", s.base, s.quote),
        _ => s.to_string(),
    }
}

/// Subscribe frame for a venue's funding stream (Binance needs none: it uses
/// a combined stream URL, like its spot partial books).
pub fn perp_subscribe_frame(venue: Venue, symbols: &[Symbol]) -> Value {
    match venue {
        Venue::Bybit => serde_json::json!({
            "op": "subscribe",
            "args": symbols.iter().map(|s| format!("tickers.{}", perp_spelling(venue, s))).collect::<Vec<_>>(),
        }),
        Venue::Okx => serde_json::json!({
            "op": "subscribe",
            "args": symbols.iter().map(|s| {
                serde_json::json!({"channel": "funding-rate", "instId": perp_spelling(venue, s)})
            }).collect::<Vec<_>>(),
        }),
        _ => Value::Null,
    }
}

/// Match a stream symbol back to a configured spot `Symbol`.
fn match_symbol(venue: Venue, symbols: &[Symbol], stream_sym: &str) -> Option<Symbol> {
    symbols
        .iter()
        .find(|s| perp_spelling(venue, s) == stream_sym)
        .cloned()
}

fn ms(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Binance `markPriceUpdate`: `r` is the rate per 8 h interval, `T` the next
/// funding time. Returns `None` for non-funding messages on the socket.
pub fn parse_binance_funding(
    venue_symbols: &[Symbol],
    raw: &str,
) -> Result<Option<(Symbol, FundingRate)>, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    // Combined streams wrap the payload: {"stream": "...", "data": {...}}.
    let d = v.get("data").unwrap_or(&v);
    if d["e"].as_str() != Some("markPriceUpdate") {
        return Ok(None);
    }
    let stream_sym = d["s"].as_str().unwrap_or("").to_uppercase();
    let Some(sym) = venue_symbols
        .iter()
        .find(|s| perp_spelling(Venue::Binance, s).to_uppercase() == stream_sym)
        .cloned()
    else {
        return Ok(None);
    };
    let rate = num(&d["r"])?;
    let next = ms(&d["T"]).unwrap_or(0);
    Ok(Some((
        sym.clone(),
        FundingRate {
            venue: Venue::Binance,
            symbol: sym,
            rate_per_interval: rate,
            interval_hours: Decimal::new(8, 0),
            next_funding_ms: next,
        },
    )))
}

/// Bybit `tickers` on the linear stream. Deltas without funding fields are
/// routine (`None`); snapshots carry `fundingRate` + `fundingIntervalHour`.
pub fn parse_bybit_funding(
    venue_symbols: &[Symbol],
    raw: &str,
) -> Result<Option<(Symbol, FundingRate)>, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    let topic = v["topic"].as_str().unwrap_or("");
    let stream_sym = match topic.strip_prefix("tickers.") {
        Some(s) => s,
        None => return Ok(None),
    };
    if v["data"]["fundingRate"].is_null() {
        return Ok(None);
    }
    let Some(sym) = match_symbol(Venue::Bybit, venue_symbols, stream_sym) else {
        return Ok(None);
    };
    let rate = num(&v["data"]["fundingRate"])?;
    let hours = num(&v["data"]["fundingIntervalHour"]).unwrap_or(Decimal::new(8, 0));
    let next = ms(&v["data"]["nextFundingTime"]).unwrap_or(0);
    Ok(Some((
        sym.clone(),
        FundingRate {
            venue: Venue::Bybit,
            symbol: sym,
            rate_per_interval: rate,
            interval_hours: hours,
            next_funding_ms: next,
        },
    )))
}

/// OKX `funding-rate` channel: `fundingRate` per `fundingInterval` (`"8H"`).
/// Event acks (`{"event": ...}`) are routine (`None`).
pub fn parse_okx_funding(
    venue_symbols: &[Symbol],
    raw: &str,
) -> Result<Option<(Symbol, FundingRate)>, FeedError> {
    let v: Value = serde_json::from_str(raw)?;
    if v.get("event").is_some() {
        return Ok(None);
    }
    let inst = v["arg"]["instId"].as_str().unwrap_or("");
    let data = match v["data"].as_array().and_then(|a| a.first()) {
        Some(d) => d,
        None => return Ok(None),
    };
    if data["fundingRate"].is_null() {
        return Ok(None);
    }
    let Some(sym) = match_symbol(Venue::Okx, venue_symbols, inst) else {
        return Ok(None);
    };
    let rate = num(&data["fundingRate"])?;
    let interval = data["fundingInterval"].as_str().unwrap_or("8H");
    let hours: Decimal = interval
        .trim_end_matches(['H', 'h'])
        .parse()
        .unwrap_or(Decimal::new(8, 0));
    let next = ms(&data["fundingTime"]).unwrap_or(0);
    Ok(Some((
        sym.clone(),
        FundingRate {
            venue: Venue::Okx,
            symbol: sym,
            rate_per_interval: rate,
            interval_hours: hours,
            next_funding_ms: next,
        },
    )))
}

fn apply_msg(
    venue: Venue,
    symbols: &[Symbol],
    raw: &str,
) -> Result<Option<(Symbol, FundingRate)>, FeedError> {
    match venue {
        Venue::Binance => parse_binance_funding(symbols, raw),
        Venue::Bybit => parse_bybit_funding(symbols, raw),
        Venue::Okx => parse_okx_funding(symbols, raw),
        _ => Ok(None),
    }
}

async fn connect_and_stream(
    venue: Venue,
    symbols: &[Symbol],
    shared: &LiveFunding,
) -> anyhow::Result<()> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let url = if venue == Venue::Binance {
        let streams: Vec<String> = symbols
            .iter()
            .map(|s| format!("{}@markPrice@1s", perp_spelling(venue, s)))
            .collect();
        format!(
            "{}/stream?streams={}",
            perp_ws_url(venue),
            streams.join("/")
        )
    } else {
        perp_ws_url(venue).to_string()
    };
    tracing::info!(venue = %venue, "dialing funding feed");
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(&url),
    )
    .await??;
    let (mut sink, mut stream) = ws.split();
    if venue != Venue::Binance {
        let frame = perp_subscribe_frame(venue, symbols);
        sink.send(Message::Text(frame.to_string().into())).await?;
    }
    tracing::info!(venue = %venue, symbols = symbols.len(), "funding feed connected");
    use futures_util::StreamExt;
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(60), stream.next()).await?;
        let Some(msg) = msg else {
            anyhow::bail!("stream closed by venue");
        };
        let msg = msg?;
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
        let now = now_ms();
        match apply_msg(venue, symbols, msg.to_text()?) {
            Ok(Some((sym, rate))) => {
                let mut map = shared.lock().unwrap_or_else(|e| e.into_inner());
                map.insert((venue, sym.to_string()), (rate, now));
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(venue = %venue, %e, "funding frame skipped"),
        }
    }
}

/// One reconnecting funding task per perp venue (Binance/Bybit/OKX).
/// Indodax has no perps and is never subscribed. Fails closed per venue.
pub fn spawn_funding_tasks(shared: &LiveFunding, venues: Vec<Venue>, symbols: Vec<Symbol>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    for v in venues {
        if !matches!(v, Venue::Binance | Venue::Bybit | Venue::Okx) {
            continue;
        }
        if perp_ws_url(v).is_empty() {
            continue;
        }
        let (shared, symbols) = (shared.clone(), symbols.clone());
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match connect_and_stream(v, &symbols, &shared).await {
                    Err(e) => {
                        tracing::warn!(venue = %v, %e, backoff_s = backoff.as_secs(), "funding feed down; reconnecting");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                    Ok(()) => backoff = Duration::from_secs(1),
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn btc() -> Symbol {
        Symbol::new("BTC", "USDT")
    }

    #[test]
    fn binance_mark_price_parses_and_heartbeat_skips() {
        let syms = vec![btc()];
        let hit = parse_binance_funding(
            &syms,
            r#"{"e":"markPriceUpdate","E":1,"s":"BTCUSDT","p":"50000","i":"1","P":"2","r":"0.000100","T":999}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(hit.1.venue, Venue::Binance);
        assert_eq!(hit.1.rate_per_interval.to_string(), "0.000100");
        assert_eq!(hit.1.interval_hours, Decimal::new(8, 0));
        assert_eq!(hit.1.next_funding_ms, 999);
        // Unknown event types and symbols are routine skips, not errors.
        assert!(
            parse_binance_funding(&syms, r#"{"e":"aggTrade","s":"BTCUSDT"}"#)
                .unwrap()
                .is_none()
        );
        assert!(parse_binance_funding(
            &syms,
            r#"{"e":"markPriceUpdate","s":"ETHUSDT","r":"0.1","T":1}"#
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn bybit_snapshot_parses_and_delta_skips() {
        let syms = vec![btc()];
        let hit = parse_bybit_funding(
            &syms,
            r#"{"topic":"tickers.BTCUSDT","type":"snapshot","ts":1,"data":{"symbol":"BTCUSDT","fundingRate":"0.000500","fundingIntervalHour":"8","nextFundingTime":"1720000000000"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(hit.1.venue, Venue::Bybit);
        assert_eq!(hit.1.rate_per_interval.to_string(), "0.000500");
        // Deltas without funding fields are routine.
        assert!(parse_bybit_funding(
            &syms,
            r#"{"topic":"tickers.BTCUSDT","type":"delta","ts":2,"data":{"symbol":"BTCUSDT","lastPrice":"50000"}}"#,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn okx_funding_rate_parses_and_ack_skips() {
        let syms = vec![btc()];
        let hit = parse_okx_funding(
            &syms,
            r#"{"arg":{"channel":"funding-rate","instId":"BTC-USDT-SWAP"},"data":[{"fundingRate":"0.000200","fundingTime":"1720000000000","fundingInterval":"8H"}]}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(hit.1.venue, Venue::Okx);
        assert_eq!(hit.1.interval_hours, Decimal::new(8, 0));
        assert!(
            parse_okx_funding(&syms, r#"{"event":"subscribe","arg":{}}"#)
                .unwrap()
                .is_none()
        );
    }
}
