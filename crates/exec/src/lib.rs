//! Live order placement seam. Ships DISABLED: `LiveExecutor` refuses to send
//! anything until constructed with `armed = true`, which the binary only does
//! when config `mode = "live"` AND venue keys exist in env. Signing helpers
//! (HMAC-SHA256 query signing used by Binance/Bybit/Indodax-style APIs) are
//! pure and unit-tested; HTTP transport is wired per-venue at go-live.

use arb_core::{Intent, Venue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
}
