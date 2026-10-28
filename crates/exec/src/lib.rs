//! Live order placement seam. Ships DISABLED: `LiveExecutor` refuses to send
//! anything until constructed with `armed = true`, which the binary only does
//! when config `mode = "live"` AND venue keys exist in env. Signing helpers
//! (HMAC-SHA256 query signing used by Binance/Bybit/Indodax-style APIs) are
//! pure and unit-tested; HTTP transport is wired per-venue at go-live.

use arb_core::{Intent, Side, TradeLeg, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VenueKeys {
    pub api_key: String,
    pub api_secret: String,
    pub passphrase: Option<String>, // OKX
}

/// Load `<VENUE>_API_KEY` / `<VENUE>_API_SECRET` (+ `OKX_API_PASSPHRASE`) from env.
/// Returns only venues with both key and secret present. Never logs values.
pub fn keys_from_env() -> BTreeMap<Venue, VenueKeys> {
    let mut out = BTreeMap::new();
    for (venue, prefix) in [
        (Venue::Binance, "BINANCE"),
        (Venue::Bybit, "BYBIT"),
        (Venue::Okx, "OKX"),
        (Venue::Indodax, "INDODAX"),
    ] {
        let (Ok(k), Ok(s)) = (
            std::env::var(format!("{prefix}_API_KEY")),
            std::env::var(format!("{prefix}_API_SECRET")),
        ) else {
            continue;
        };
        if k.is_empty() || s.is_empty() {
            continue;
        }
        let passphrase = if venue == Venue::Okx {
            std::env::var("OKX_API_PASSPHRASE").ok()
        } else {
            None
        };
        out.insert(
            venue,
            VenueKeys {
                api_key: k,
                api_secret: s,
                passphrase,
            },
        );
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecMode {
    Paper,
    Live,
}

impl ExecMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "paper" => Some(Self::Paper),
            "live" => Some(Self::Live),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("live executor not armed (mode={mode:?}, venues_with_keys={venues}); refusing to send orders")]
    NotArmed { mode: ExecMode, venues: usize },
    #[error("no keys for venue {0}")]
    NoKeys(Venue),
    #[error("refusing Binance send: '{0}' not on the testnet allowlist")]
    RejectedHost(String),
    #[error("Binance order rejected (HTTP {status}): {body}")]
    BinanceRejected { status: u16, body: String },
    #[error("Bybit order rejected (HTTP {status}): {body}")]
    BybitRejected { status: u16, body: String },
    #[error("Binance transport error: {0}")]
    Transport(String),
}

pub struct LiveExecutor {
    pub mode: ExecMode,
    keys: BTreeMap<Venue, VenueKeys>,
}

impl LiveExecutor {
    pub fn new(mode: ExecMode, keys: BTreeMap<Venue, VenueKeys>) -> Self {
        Self { mode, keys }
    }

    pub fn armed(&self) -> bool {
        self.mode == ExecMode::Live && !self.keys.is_empty()
    }

    /// Gate every live send path through here.
    pub fn guard(&self, intent: &Intent) -> Result<(), ExecError> {
        if !self.armed() {
            return Err(ExecError::NotArmed {
                mode: self.mode,
                venues: self.keys.len(),
            });
        }
        for leg in &intent.legs {
            if leg.venue.is_cex() && !self.keys.contains_key(&leg.venue) {
                return Err(ExecError::NoKeys(leg.venue));
            }
        }
        Ok(())
    }

    pub fn keys_for(&self, venue: Venue) -> Option<VenueKeys> {
        self.keys.get(&venue).cloned()
    }
}

// --- Binance spot testnet sender. Best-effort: the engine keeps its paper
// fill no matter what this returns. ---

/// Default REST base for Binance spot testnet; override with `BINANCE_API_BASE`.
pub const BINANCE_TESTNET_BASE: &str = "https://testnet.binance.vision";
/// Fail-closed allowlist: signed order requests go only to this host over HTTPS.
const BINANCE_HOSTS: &[&str] = &["testnet.binance.vision"];

/// REST base for Binance order sends (`BINANCE_API_BASE`, else the testnet default).
pub fn binance_api_base() -> String {
    std::env::var("BINANCE_API_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| BINANCE_TESTNET_BASE.to_string())
}

/// Full `POST {base}/api/v3/order?...` URL with the query signed by [`sign_query`].
/// Tiny GTC LIMIT order mirroring the paper-filled leg. Pure: no network.
/// Fails closed on any base outside the testnet allowlist.
pub fn binance_order_url(
    leg: &TradeLeg,
    api_secret: &str,
    base: &str,
    now_ms: i64,
) -> Result<String, ExecError> {
    let url = reqwest::Url::parse(base).map_err(|_| ExecError::RejectedHost(base.to_string()))?;
    let host = url.host_str().unwrap_or_default().to_string();
    if url.scheme() != "https" || !BINANCE_HOSTS.contains(&host.as_str()) {
        return Err(ExecError::RejectedHost(
            url.host_str().unwrap_or(base).to_string(),
        ));
    }
    let side = match leg.side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    };
    let query = format!(
        "symbol={}{}&side={side}&type=LIMIT&timeInForce=GTC&quantity={}&price={}&timestamp={now_ms}",
        leg.symbol.base, leg.symbol.quote, leg.qty_base, leg.price
    );
    Ok(format!(
        "{}/api/v3/order?{}",
        base.trim_end_matches('/'),
        sign_query(api_secret, &query)
    ))
}

/// Send one paper-filled Binance leg as a LIMIT order. Returns the raw body on
/// 2xx. Call only after [`LiveExecutor::guard`] passes; tests cover URL shape
/// instead (no network in tests).
pub async fn send_binance_limit(
    leg: &TradeLeg,
    keys: &VenueKeys,
    base: &str,
    now_ms: i64,
) -> Result<String, ExecError> {
    let url = binance_order_url(leg, &keys.api_secret, base, now_ms)?;
    // Same explicit ring provider as arb-feed's live WS tasks.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ExecError::Transport(e.to_string()))?;
    let resp = client
        .post(url)
        .header("X-MBX-APIKEY", &keys.api_key)
        .send()
        .await
        .map_err(|e| ExecError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ExecError::Transport(e.to_string()))?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(ExecError::BinanceRejected {
            status: status.as_u16(),
            body,
        })
    }
}

// --- Bybit V5 spot testnet sender. Same best-effort contract as Binance. ---
// Auth: `X-BAPI-API-KEY` + `X-BAPI-TIMESTAMP` + `X-BAPI-RECV-WINDOW` +
// `X-BAPI-SIGN` (HMAC-SHA256 hex of `timestamp + api_key + recv_window + body`).

/// Default REST base for Bybit testnet; override with `BYBIT_API_BASE`.
pub const BYBIT_TESTNET_BASE: &str = "https://api-testnet.bybit.com";
/// Fail-closed allowlist: signed order requests go only to this host over HTTPS.
const BYBIT_HOSTS: &[&str] = &["api-testnet.bybit.com"];
/// Fixed receive window (ms) sent with every Bybit order.
pub const BYBIT_RECV_WINDOW: &str = "5000";

/// REST base for Bybit order sends (`BYBIT_API_BASE`, else the testnet default).
pub fn bybit_api_base() -> String {
    std::env::var("BYBIT_API_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| BYBIT_TESTNET_BASE.to_string())
}

/// Full `POST {base}/v5/order/create` endpoint. Pure: no network.
/// Fails closed on any base outside the testnet allowlist.
pub fn bybit_order_endpoint(base: &str) -> Result<String, ExecError> {
    let url = reqwest::Url::parse(base).map_err(|_| ExecError::RejectedHost(base.to_string()))?;
    let host = url.host_str().unwrap_or_default().to_string();
    if url.scheme() != "https" || !BYBIT_HOSTS.contains(&host.as_str()) {
        return Err(ExecError::RejectedHost(
            url.host_str().unwrap_or(base).to_string(),
        ));
    }
    Ok(format!("{}/v5/order/create", base.trim_end_matches('/')))
}

/// Tiny GTC LIMIT order body mirroring the paper-filled leg. Pure: no network.
/// Takes already-quantized price/qty (see [`round_to_tick`]).
pub fn bybit_order_body(leg: &TradeLeg, qty: Decimal, price: Decimal) -> String {
    let side = match leg.side {
        Side::Buy => "Buy",
        Side::Sell => "Sell",
    };
    serde_json::json!({
        "category": "spot",
        "symbol": format!("{}{}", leg.symbol.base, leg.symbol.quote),
        "side": side,
        "orderType": "Limit",
        "qty": qty.to_string(),
        "price": price.to_string(),
        "timeInForce": "GTC",
    })
    .to_string()
}

/// Bybit answers logic errors with HTTP 200 plus a nonzero `retCode`, so a
/// bare 2xx check reports failures as sent. Pure: split for tests.
pub fn bybit_check_response(status: u16, body: &str) -> Result<String, ExecError> {
    let ok = (200..300).contains(&status)
        && serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("retCode")?.as_i64())
            == Some(0);
    if ok {
        Ok(body.to_string())
    } else {
        Err(ExecError::BybitRejected {
            status,
            body: body.to_string(),
        })
    }
}

/// Round down to a tick multiple (floor never exceeds inventory or budget).
/// Non-positive tick = no venue info; return the value untouched. Pure.
pub fn round_to_tick(value: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return value;
    }
    (value / tick).floor() * tick
}

/// (price tick, qty step) per spot symbol, fetched once from
/// instruments-info and cached for the process lifetime.
static BYBIT_TICKS: OnceLock<Mutex<HashMap<String, (Decimal, Decimal)>>> = OnceLock::new();

/// Price tick + qty step for a spot symbol, via `GET
/// /v5/market/instruments-info`. Cached per process; `None` on any failure,
/// in which case the caller sends unquantized (still best-effort).
async fn bybit_tick_info(
    client: &reqwest::Client,
    base: &str,
    symbol: &str,
) -> Option<(Decimal, Decimal)> {
    if let Some(hit) = BYBIT_TICKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|m| m.get(symbol).copied())
    {
        return Some(hit);
    }
    // Reuse the order-send gate: same host, same HTTPS rule.
    bybit_order_endpoint(base).ok()?;
    let url = format!(
        "{}/v5/market/instruments-info?category=spot&symbol={symbol}",
        base.trim_end_matches('/')
    );
    let resp = client.get(url).send().await.ok()?;
    let status = resp.status().as_u16();
    let body = resp.text().await.ok()?;
    let body = bybit_check_response(status, &body).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let info = v.get("result")?.get("list")?.get(0)?;
    let tick = info
        .get("priceFilter")?
        .get("tickSize")?
        .as_str()?
        .parse::<Decimal>()
        .ok()?;
    let step = info
        .get("lotSizeFilter")?
        .get("qtyStep")?
        .as_str()?
        .parse::<Decimal>()
        .ok()?;
    if tick <= Decimal::ZERO || step <= Decimal::ZERO {
        return None;
    }
    BYBIT_TICKS
        .get()?
        .lock()
        .ok()?
        .insert(symbol.to_string(), (tick, step));
    Some((tick, step))
}

/// Bybit V5 signature: `hmac(secret, timestamp + api_key + recv_window + body)`.
pub fn bybit_sign(api_secret: &str, timestamp_ms: i64, api_key: &str, body: &str) -> String {
    let payload = format!("{timestamp_ms}{api_key}{}{body}", BYBIT_RECV_WINDOW);
    hmac_sha256_hex(api_secret.as_bytes(), payload.as_bytes())
}

/// Send one paper-filled Bybit leg as a LIMIT order. Returns the raw body on
/// success (`retCode == 0`). Call only after [`LiveExecutor::guard`] passes;
/// tests cover body shape and response handling instead (no network in tests).
pub async fn send_bybit_limit(
    leg: &TradeLeg,
    keys: &VenueKeys,
    base: &str,
    now_ms: i64,
) -> Result<String, ExecError> {
    let url = bybit_order_endpoint(base)?;
    // Same explicit ring provider as arb-feed's live WS tasks.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| ExecError::Transport(e.to_string()))?;
    // Quantize to venue ticks; unquantized when the lookup fails.
    let symbol = format!("{}{}", leg.symbol.base, leg.symbol.quote);
    let (price, qty) = match bybit_tick_info(&client, base, &symbol).await {
        Some((tick, step)) => (
            round_to_tick(leg.price, tick),
            round_to_tick(leg.qty_base, step),
        ),
        None => (leg.price, leg.qty_base),
    };
    if price <= Decimal::ZERO || qty <= Decimal::ZERO {
        // shortcut: dust below the venue minimum; reported, never sent.
        return Err(ExecError::BybitRejected {
            status: 0,
            body: "price or quantity below venue minimum after tick rounding".into(),
        });
    }
    let body = bybit_order_body(leg, qty, price);
    let sign = bybit_sign(&keys.api_secret, now_ms, &keys.api_key, &body);
    let resp = client
        .post(url)
        .header("X-BAPI-API-KEY", &keys.api_key)
        .header("X-BAPI-TIMESTAMP", now_ms.to_string())
        .header("X-BAPI-RECV-WINDOW", BYBIT_RECV_WINDOW)
        .header("X-BAPI-SIGN", sign)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| ExecError::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| ExecError::Transport(e.to_string()))?;
    bybit_check_response(status, &body)
}

// --- HMAC-SHA256 via the audited RustCrypto `sha2` + `hmac` crates. ---

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Binance/Indodax-style signed query: `qs&signature=hmac(secret, qs)`.
pub fn sign_query(secret: &str, query: &str) -> String {
    format!(
        "{query}&signature={}",
        hmac_sha256_hex(secret.as_bytes(), query.as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{MarketKind, Side, Symbol, TradeLeg};
    use rust_decimal_macros::dec;

    #[test]
    fn rfc4231_hmac_vector() {
        // Test Case 2: key "Jefe", data "what do ya want for nothing?"
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn paper_mode_refuses_live_sends() {
        let ex = LiveExecutor::new(ExecMode::Paper, BTreeMap::new());
        let intent = Intent {
            strategy: "t".into(),
            expected_edge_bps: dec!(50),
            ttl_ms: 100,
            created_ms: 0,
            roundtrip: true,
            legs: vec![TradeLeg {
                venue: Venue::Binance,
                symbol: Symbol::new("BTC", "USDT"),
                side: Side::Buy,
                price: dec!(1),
                qty_base: dec!(1),
                market: MarketKind::Spot,
            }],
        };
        assert!(matches!(ex.guard(&intent), Err(ExecError::NotArmed { .. })));
        assert!(sign_query("s", "a=1").starts_with("a=1&signature="));
    }

    fn buy_leg() -> TradeLeg {
        TradeLeg {
            venue: Venue::Binance,
            symbol: Symbol::new("BTC", "USDT"),
            side: Side::Buy,
            price: dec!(80000),
            qty_base: dec!(0.001),
            market: MarketKind::Spot,
        }
    }

    #[test]
    fn binance_order_url_shapes_signed_limit() {
        let url = binance_order_url(
            &buy_leg(),
            "secret",
            BINANCE_TESTNET_BASE,
            1_700_000_000_000,
        )
        .unwrap();
        assert!(
            url.starts_with("https://testnet.binance.vision/api/v3/order?"),
            "{url}"
        );
        for part in [
            "symbol=BTCUSDT",
            "side=BUY",
            "type=LIMIT",
            "timeInForce=GTC",
            "quantity=0.001",
            "price=80000",
            "timestamp=1700000000000",
        ] {
            assert!(url.contains(part), "{url}");
        }
        // Signature reuses sign_query over the same unsigned query.
        let qs = url.split('?').nth(1).unwrap();
        let unsigned = qs.rsplit_once("&signature=").unwrap().0;
        assert_eq!(qs, sign_query("secret", unsigned));
        let sig = qs.rsplit_once("&signature=").unwrap().1;
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn binance_allowlist_refuses_non_testnet() {
        let leg = buy_leg();
        for base in [
            "https://api.binance.com",       // mainnet
            "http://testnet.binance.vision", // plain HTTP to the right host
            "https://evil-testnet.binance.vision.evil.com",
            "not a url",
        ] {
            assert!(
                matches!(
                    binance_order_url(&leg, "s", base, 1),
                    Err(ExecError::RejectedHost(_))
                ),
                "{base}"
            );
        }
    }

    fn bybit_buy_leg() -> TradeLeg {
        TradeLeg {
            venue: Venue::Bybit,
            symbol: Symbol::new("BTC", "USDT"),
            side: Side::Buy,
            price: dec!(80000),
            qty_base: dec!(0.001),
            market: MarketKind::Spot,
        }
    }

    #[test]
    fn bybit_order_body_shapes_limit() {
        let leg = bybit_buy_leg();
        let body = bybit_order_body(&leg, leg.qty_base, leg.price);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["category"], "spot");
        assert_eq!(v["symbol"], "BTCUSDT");
        assert_eq!(v["side"], "Buy");
        assert_eq!(v["orderType"], "Limit");
        assert_eq!(v["qty"], "0.001");
        assert_eq!(v["price"], "80000");
        assert_eq!(v["timeInForce"], "GTC");
        let endpoint = bybit_order_endpoint(BYBIT_TESTNET_BASE).unwrap();
        assert_eq!(endpoint, "https://api-testnet.bybit.com/v5/order/create");
        // Signature reuses the timestamp + key + window + body payload.
        let sig = bybit_sign("secret", 1_700_000_000_000, "key", &body);
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        let payload = format!("1700000000000key5000{body}");
        assert_eq!(sig, hmac_sha256_hex(b"secret", payload.as_bytes()));
    }

    #[test]
    fn bybit_rejects_nonzero_retcode_on_http_200() {
        let ok = r#"{"retCode":0,"retMsg":"OK","result":{"orderId":"x"}}"#;
        assert!(bybit_check_response(200, ok).is_ok());
        let bad = r#"{"retCode":10001,"retMsg":"price invalid."}"#;
        assert!(matches!(
            bybit_check_response(200, bad),
            Err(ExecError::BybitRejected { status: 200, .. })
        ));
        assert!(bybit_check_response(400, bad).is_err());
        assert!(bybit_check_response(200, "not json").is_err());
    }

    #[test]
    fn round_to_tick_floors_to_multiple() {
        assert_eq!(
            round_to_tick(dec!(0.103769242), Decimal::new(1, 5)),
            dec!(0.10376)
        );
        assert_eq!(round_to_tick(dec!(80000), dec!(1)), dec!(80000));
        assert_eq!(round_to_tick(dec!(0.5), Decimal::ZERO), dec!(0.5));
    }

    #[test]
    fn bybit_allowlist_refuses_non_testnet() {
        for base in [
            "https://api.bybit.com",        // mainnet
            "http://api-testnet.bybit.com", // plain HTTP to the right host
            "https://api-testnet.bybit.com.evil.com",
            "not a url",
        ] {
            assert!(
                matches!(bybit_order_endpoint(base), Err(ExecError::RejectedHost(_))),
                "{base}"
            );
        }
    }
}
