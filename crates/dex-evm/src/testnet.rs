//! Sepolia TESTNET send probe over HTTPS JSON-RPC (`EVM_RPC_URL` only).
//!
//! FAIL-CLOSED: `eth_chainId` must be [`SEPOLIA_CHAIN_ID`] and `to` must be a
//! known Sepolia Uniswap address before anything is signed or sent.
//!
//! Signing uses `k256` (pure-Rust secp256k1, no C toolchain) + `sha3`
//! Keccak-256; legacy-tx RLP and V2 calldata are hand-rolled (~60 lines —
//! smaller than pulling `alloy`/`rlp`). Secrets (the RPC URL may embed an API
//! key; `key_hex` is a raw private key) never appear in errors or logs.

use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use serde_json::Value;
use sha3::{Digest, Keccak256};
use std::time::Duration;

/// Sepolia chain ID. Any other `eth_chainId` aborts the send.
pub const SEPOLIA_CHAIN_ID: u64 = 11155111;
/// Sepolia Uniswap V2 Router02 — official V2 deployments page. NOTE: this is
/// NOT the mainnet router (`0x7a25...`); same-address-everywhere is false.
/// <https://developers.uniswap.org/docs/protocols/v2/deployments>
pub const SEPOLIA_UNISWAP_V2_ROUTER: &str = "0xeE567Fe1712Faf6149d80dA1E6934E354124CfE3";
/// Sepolia Uniswap V2 factory (same page as above).
pub const SEPOLIA_UNISWAP_V2_FACTORY: &str = "0xF62c03E08ada871A0bEb309762E260a7a6a880E6";
/// Sepolia Uniswap V3 QuoterV2 — official V3 Ethereum-deployments page (the
/// mainnet Quoter `0x61fFE...` is not deployed here).
/// <https://developers.uniswap.org/docs/protocols/v3/deployments/v3-ethereum-deployments>
pub const SEPOLIA_UNISWAP_V3_QUOTER_V2: &str = "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3";
/// Sepolia Uniswap V3 SwapRouter02 (same page as above).
pub const SEPOLIA_UNISWAP_V3_SWAP_ROUTER_02: &str = "0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E";

/// Fail-closed destination allowlist: only these Sepolia contracts.
const ALLOWLIST: &[&str] = &[
    SEPOLIA_UNISWAP_V2_ROUTER,
    SEPOLIA_UNISWAP_V2_FACTORY,
    SEPOLIA_UNISWAP_V3_QUOTER_V2,
    SEPOLIA_UNISWAP_V3_SWAP_ROUTER_02,
];

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("EVM_RPC_URL is unset or empty; refusing to send")]
    MissingRpcUrl,
    #[error("refusing send: EVM_RPC_URL must be https")]
    NonHttpsRpcUrl,
    #[error("refusing send: '{0}' not on the Sepolia allowlist")]
    NotAllowlisted(String),
    #[error("refusing send: chain {0} is not Sepolia (11155111)")]
    WrongChain(u64),
    #[error("bad hex address or key")]
    BadHex,
    #[error("signing failed")]
    Sign,
    #[error("RPC method {method} failed: {detail}")]
    Rpc {
        method: &'static str,
        detail: String,
    },
    #[error("transport error: {0}")]
    Transport(String),
}

/// Endpoint for every send path. ONLY `EVM_RPC_URL` — never a literal, arg, or WSS URL.
pub fn rpc_url_from_env() -> Result<String, SendError> {
    let url = std::env::var("EVM_RPC_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or(SendError::MissingRpcUrl)?;
    ensure_https_rpc(&url)
}

/// Fail-closed scheme gate: https only, so keys and txs never cross plaintext.
pub fn ensure_https_rpc(url: &str) -> Result<String, SendError> {
    let url = url.trim();
    if url.starts_with("https://") {
        Ok(url.to_string())
    } else {
        Err(SendError::NonHttpsRpcUrl)
    }
}

/// Fail-closed chain gate: Sepolia or nothing.
pub fn ensure_sepolia_chain_id(chain_id: u64) -> Result<(), SendError> {
    if chain_id == SEPOLIA_CHAIN_ID {
        Ok(())
    } else {
        Err(SendError::WrongChain(chain_id))
    }
}

/// Fail-closed destination gate (case-insensitive hex compare).
pub fn ensure_allowlisted(to: &str) -> Result<(), SendError> {
    let norm = to.to_lowercase();
    if ALLOWLIST.iter().any(|a| a.to_lowercase() == norm) {
        Ok(())
    } else {
        Err(SendError::NotAllowlisted(to.to_string()))
    }
}

fn strip_hex(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
}

fn hex_val(c: u8) -> Result<u8, SendError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(SendError::BadHex),
    }
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>, SendError> {
    let h = strip_hex(s);
    if !h.len().is_multiple_of(2) {
        return Err(SendError::BadHex);
    }
    let (pairs, _) = h.as_bytes().as_chunks::<2>();
    pairs
        .iter()
        .map(|c| Ok(hex_val(c[0])? << 4 | hex_val(c[1])?))
        .collect()
}

fn hex_to_20(s: &str) -> Result<[u8; 20], SendError> {
    let b = hex_to_bytes(s)?;
    b.try_into().map_err(|_| SendError::BadHex)
}

fn bytes_to_hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for byte in b {
        s.push(H[(byte >> 4) as usize] as char);
        s.push(H[(byte & 15) as usize] as char);
    }
    s
}

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// Ethereum address of a raw private key (`key_hex`, 32 bytes): keccak of the
/// uncompressed pubkey sans prefix, last 20 bytes.
pub fn address_from_key(key_hex: &str) -> Result<[u8; 20], SendError> {
    let sk = SigningKey::from_slice(&hex_to_bytes(key_hex)?).map_err(|_| SendError::Sign)?;
    let pt = sk.verifying_key().to_sec1_point(false);
    let hash = keccak256(&pt.as_bytes()[1..]);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&hash[12..]);
    Ok(addr)
}

// --- Minimal RLP (legacy txs only): short strings/lists; no length prefixes
// over 255 bytes, which a 9-field tx never reaches. ---

fn be_min(mut v: u128, out: &mut Vec<u8>) {
    if v == 0 {
        return;
    }
    let mut buf = [0u8; 16];
    for i in (0..16).rev() {
        buf[i] = v as u8;
        v >>= 8;
    }
    let skip = buf.iter().take_while(|b| **b == 0).count();
    out.extend_from_slice(&buf[skip..]);
}

fn rlp_bytes(out: &mut Vec<u8>, b: &[u8]) {
    if b.len() == 1 && b[0] < 0x80 {
        out.push(b[0]);
    } else if b.len() <= 55 {
        out.push(0x80 + b.len() as u8);
        out.extend_from_slice(b);
    } else {
        let mut len = Vec::new();
        be_min(b.len() as u128, &mut len);
        out.push(0xb7 + len.len() as u8);
        out.extend_from_slice(&len);
        out.extend_from_slice(b);
    }
}

/// Minimal big-endian for RLP ints (no leading zeros); keep ≥1 byte.
fn nz(mut b: &[u8]) -> &[u8] {
    while b.len() > 1 && b[0] == 0 {
        b = &b[1..];
    }
    b
}

fn rlp_list(out: &mut Vec<u8>, payload: &[u8]) {
    if payload.len() <= 55 {
        out.push(0xc0 + payload.len() as u8);
    } else {
        let mut len = Vec::new();
        be_min(payload.len() as u128, &mut len);
        out.push(0xf7 + len.len() as u8);
        out.extend_from_slice(&len);
    }
    out.extend_from_slice(payload);
}

/// Unsigned legacy send. `data` empty for plain value transfers.
#[derive(Debug, Clone)]
pub struct LegacyTx {
    pub nonce: u64,
    pub gas_price_wei: u128,
    pub gas_limit: u64,
    pub to: [u8; 20],
    pub value_wei: u128,
    pub data: Vec<u8>,
}

fn rlp_legacy(tx: &LegacyTx, chain_id: u64, sig: Option<(u64, &[u8], &[u8])>) -> Vec<u8> {
    let mut p = Vec::new();
    let mut n = Vec::new();
    be_min(tx.nonce as u128, &mut n);
    rlp_bytes(&mut p, &n);
    let mut g = Vec::new();
    be_min(tx.gas_price_wei, &mut g);
    rlp_bytes(&mut p, &g);
    let mut l = Vec::new();
    be_min(tx.gas_limit as u128, &mut l);
    rlp_bytes(&mut p, &l);
    rlp_bytes(&mut p, &tx.to);
    let mut v = Vec::new();
    be_min(tx.value_wei, &mut v);
    rlp_bytes(&mut p, &v);
    rlp_bytes(&mut p, &tx.data);
    match sig {
        None => {
            // EIP-155 signing payload: chain_id, 0, 0.
            let mut c = Vec::new();
            be_min(chain_id as u128, &mut c);
            rlp_bytes(&mut p, &c);
            rlp_bytes(&mut p, &[]);
            rlp_bytes(&mut p, &[]);
        }
        Some((vv, r, s)) => {
            let mut c = Vec::new();
            be_min(vv as u128, &mut c);
            rlp_bytes(&mut p, &c);
            rlp_bytes(&mut p, r);
            rlp_bytes(&mut p, s);
        }
    }
    let mut out = Vec::new();
    rlp_list(&mut out, &p);
    out
}

/// EIP-155 signing hash: keccak of the unsigned RLP.
pub fn eip155_sighash(tx: &LegacyTx, chain_id: u64) -> [u8; 32] {
    keccak256(&rlp_legacy(tx, chain_id, None))
}

/// Sign with a raw private key; returns the signed RLP ready for
/// `eth_sendRawTransaction`.
pub fn sign_legacy_tx(tx: &LegacyTx, chain_id: u64, key_hex: &str) -> Result<Vec<u8>, SendError> {
    let sk = SigningKey::from_slice(&hex_to_bytes(key_hex)?).map_err(|_| SendError::Sign)?;
    let hash = eip155_sighash(tx, chain_id);
    // Raw prehash: k256's Digest-bound API wants digest-0.11 while sha3-0.10
    // speaks digest-0.10, so sign the 32 bytes directly instead of adapting.
    let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&hash);
    let raw = sig.to_bytes();
    let v = chain_id * 2 + 35 + recid.to_byte() as u64;
    Ok(rlp_legacy(
        tx,
        chain_id,
        Some((v, nz(&raw[..32]), nz(&raw[32..]))),
    ))
}

/// `swapExactETHForTokens(uint,address[],address,uint)` calldata. Selector
/// `7ff36ab5` = keccak of the signature, first 4 bytes.
pub fn v2_swap_exact_eth_calldata(
    amount_out_min_wei: u128,
    path: &[[u8; 20]],
    to: &[u8; 20],
    deadline_unix: u64,
) -> Vec<u8> {
    fn u256(v: u128, out: &mut Vec<u8>) {
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn addr(a: &[u8; 20], out: &mut Vec<u8>) {
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(a);
    }
    let mut out = vec![0x7f, 0xf3, 0x6a, 0xb5];
    u256(amount_out_min_wei, &mut out);
    u256(0x80, &mut out); // path offset: after the 4 head words
    addr(to, &mut out);
    u256(deadline_unix as u128, &mut out);
    u256(path.len() as u128, &mut out);
    for hop in path {
        addr(hop, &mut out);
    }
    out
}

// --- HTTPS JSON-RPC (reqwest + rustls, same setup as arb-exec). ---

async fn rpc_call(
    client: &reqwest::Client,
    rpc_url: &str,
    method: &'static str,
    params: Value,
) -> Result<Value, SendError> {
    // URL and body may carry secrets: never echo them in errors.
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    });
    let resp = client
        .post(rpc_url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body).map_err(|e| SendError::Transport(e.to_string()))?)
        .send()
        .await
        .map_err(|e| SendError::Transport(e.without_url().to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| SendError::Transport(e.without_url().to_string()))?;
    let v: Value = serde_json::from_str(&text).map_err(|_| SendError::Rpc {
        method,
        detail: format!("non-JSON response (HTTP {status})"),
    })?;
    if let Some(err) = v.get("error") {
        return Err(SendError::Rpc {
            method,
            detail: err.to_string().chars().take(200).collect(),
        });
    }
    v.get("result").cloned().ok_or(SendError::Rpc {
        method,
        detail: "missing result".into(),
    })
}

fn parse_hex_u128(method: &'static str, v: &Value) -> Result<u128, SendError> {
    let s = v.as_str().ok_or(SendError::Rpc {
        method,
        detail: "non-string quantity".into(),
    })?;
    u128::from_str_radix(strip_hex(s), 16).map_err(|_| SendError::Rpc {
        method,
        detail: "bad hex quantity".into(),
    })
}

/// Send one allowlisted Sepolia tx (value + calldata) and return its `0x` hash.
///
/// Reads the endpoint ONLY from `EVM_RPC_URL`. Fails closed unless
/// `eth_chainId` is Sepolia and `to` is a known Sepolia Uniswap address.
pub async fn send_sepolia_tx(
    key_hex: &str,
    to: &str,
    value_wei: u128,
    data: Vec<u8>,
    gas_limit: u64,
) -> Result<String, SendError> {
    ensure_allowlisted(to)?;
    let rpc_url = rpc_url_from_env()?;
    // Same explicit ring provider as arb-exec's sender.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| SendError::Transport(e.without_url().to_string()))?;
    let chain_id = parse_hex_u128(
        "eth_chainId",
        &rpc_call(&client, &rpc_url, "eth_chainId", Value::Array(vec![])).await?,
    )?;
    let chain_id = u64::try_from(chain_id).map_err(|_| SendError::Rpc {
        method: "eth_chainId",
        detail: "chain id too large".into(),
    })?;
    ensure_sepolia_chain_id(chain_id)?;
    let from = bytes_to_hex(&address_from_key(key_hex)?);
    let nonce = parse_hex_u128(
        "eth_getTransactionCount",
        &rpc_call(
            &client,
            &rpc_url,
            "eth_getTransactionCount",
            serde_json::json!([from, "latest"]),
        )
        .await?,
    )? as u64;
    let gas_price = parse_hex_u128(
        "eth_gasPrice",
        &rpc_call(&client, &rpc_url, "eth_gasPrice", Value::Array(vec![])).await?,
    )?;
    let tx = LegacyTx {
        nonce,
        gas_price_wei: gas_price,
        gas_limit,
        to: hex_to_20(to)?,
        value_wei,
        data,
    };
    let raw = sign_legacy_tx(&tx, SEPOLIA_CHAIN_ID, key_hex)?;
    let hash = rpc_call(
        &client,
        &rpc_url,
        "eth_sendRawTransaction",
        serde_json::json!([bytes_to_hex(&raw)]),
    )
    .await?;
    hash.as_str().map(str::to_string).ok_or(SendError::Rpc {
        method: "eth_sendRawTransaction",
        detail: "non-string hash".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr;
    use k256::ecdsa::VerifyingKey;

    #[test]
    fn sepolia_router_differs_from_mainnet() {
        // The whole point of per-chain constants: the V2 router is NOT at the
        // mainnet address on Sepolia (official V2 deployments page).
        assert_ne!(
            SEPOLIA_UNISWAP_V2_ROUTER.to_lowercase(),
            addr::UNISWAP_V2_ROUTER.to_lowercase()
        );
        assert_ne!(
            SEPOLIA_UNISWAP_V3_QUOTER_V2.to_lowercase(),
            addr::UNISWAP_V3_QUOTER.to_lowercase()
        );
    }

    #[test]
    fn chain_gate_only_accepts_sepolia() {
        assert!(ensure_sepolia_chain_id(SEPOLIA_CHAIN_ID).is_ok());
        assert_eq!(SEPOLIA_CHAIN_ID, 11155111);
        for bad in [0, 1, 137, 56, 11155111 + 1] {
            assert!(
                matches!(ensure_sepolia_chain_id(bad), Err(SendError::WrongChain(c)) if c == bad),
                "{bad}"
            );
        }
    }

    #[test]
    fn allowlist_takes_sepolia_refuses_mainnet() {
        for ok in [
            SEPOLIA_UNISWAP_V2_ROUTER,
            SEPOLIA_UNISWAP_V2_FACTORY,
            SEPOLIA_UNISWAP_V3_QUOTER_V2,
            SEPOLIA_UNISWAP_V3_SWAP_ROUTER_02,
            // Case-insensitive: checksummed input still matches.
            "0xee567fe1712faf6149d80da1e6934e354124cfe3",
        ] {
            assert!(ensure_allowlisted(ok).is_ok(), "{ok}");
        }
        for bad in [
            addr::UNISWAP_V2_ROUTER, // mainnet router: right contract, wrong chain
            "0x0000000000000000000000000000000000000000",
            "0xeE567Fe1712Faf6149d80dA1E6934E354124CfE4", // last-nibble typo
            "not an address",
        ] {
            assert!(
                matches!(ensure_allowlisted(bad), Err(SendError::NotAllowlisted(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn keccak_matches_empty_string_vector() {
        // If this fails the hash fn is SHA3-256, not Ethereum Keccak-256.
        assert_eq!(
            bytes_to_hex(&keccak256(b"")),
            "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    // Private key 1 -> generator point G (canonical toy vector).
    const TEST_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
    const TEST_ADDR: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";

    fn test_tx() -> LegacyTx {
        LegacyTx {
            nonce: 0,
            gas_price_wei: 1_000_000_000,
            gas_limit: 21_000,
            to: hex_to_20(SEPOLIA_UNISWAP_V2_ROUTER).unwrap(),
            value_wei: 1,
            data: vec![],
        }
    }

    #[test]
    fn key_derives_known_address() {
        assert_eq!(
            bytes_to_hex(&address_from_key(TEST_KEY).unwrap()),
            TEST_ADDR
        );
        assert!(address_from_key("0x1234").is_err());
    }

    #[test]
    fn signature_recovers_sender_and_uses_eip155_v() {
        let tx = test_tx();
        let raw = sign_legacy_tx(&tx, SEPOLIA_CHAIN_ID, TEST_KEY).unwrap();
        // Recover the signer from (signing hash, sig) and compare addresses.
        let hash = eip155_sighash(&tx, SEPOLIA_CHAIN_ID);
        let sk = SigningKey::from_slice(&hex_to_bytes(TEST_KEY).unwrap()).unwrap();
        let (sig, recid) = sk.sign_prehash_recoverable(&hash);
        let recovered = VerifyingKey::recover_from_prehash(&hash, &sig, recid).unwrap();
        assert_eq!(&recovered, sk.verifying_key());
        let pt = recovered.to_sec1_point(false);
        let h = keccak256(&pt.as_bytes()[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&h[12..]);
        assert_eq!(bytes_to_hex(&addr), TEST_ADDR);
        // EIP-155 v = chain*2 + 35 + recid.
        let v = SEPOLIA_CHAIN_ID * 2 + 35 + recid.to_byte() as u64;
        assert!(v == SEPOLIA_CHAIN_ID * 2 + 35 || v == SEPOLIA_CHAIN_ID * 2 + 36);
        assert!(!raw.is_empty() && raw[0] >= 0xc0);
        // Deterministic (RFC6979): same inputs, same bytes.
        assert_eq!(
            raw,
            sign_legacy_tx(&tx, SEPOLIA_CHAIN_ID, TEST_KEY).unwrap()
        );
    }

    #[test]
    fn v2_calldata_has_router_selector_and_abi_shape() {
        // Shape-only test: any two valid 20-byte hops exercise the encoding.
        let weth = hex_to_20("0xfFf9976782d46d4aB515e9e4eB9eda9FBb4BA964").unwrap();
        let token = hex_to_20("0x0000000000000000000000000000000000000001").unwrap();
        let to = hex_to_20(TEST_ADDR).unwrap();
        let data = v2_swap_exact_eth_calldata(0, &[weth, token], &to, 1_800_000_000);
        assert_eq!(&data[..4], &[0x7f, 0xf3, 0x6a, 0xb5]);
        assert_eq!(data.len(), 4 + 32 * 7); // amount + offset + to + deadline + len + 2 hops
        assert_eq!(&data[4 + 32 * 2 + 12..4 + 32 * 3], &to); // head `to` word
        assert_eq!(&data[data.len() - 20..], &token); // last hop
    }

    #[test]
    fn unsigned_rlp_matches_hand_computed_golden() {
        // Hand-computed per the RLP spec: e6 + 80 + 843b9aca00 + 825208 +
        // 94<router> + 01 + 80 + 83aa36a7 + 80 + 80 (38-byte payload).
        assert_eq!(
            bytes_to_hex(&rlp_legacy(&test_tx(), SEPOLIA_CHAIN_ID, None)),
            concat!(
                "0xe680843b9aca0082520894",
                "ee567fe1712faf6149d80da1e6934e354124cfe3",
                "018083aa36a78080"
            )
        );
    }

    #[test]
    fn bad_key_never_signs() {
        assert!(matches!(
            sign_legacy_tx(&test_tx(), SEPOLIA_CHAIN_ID, "0xzzzz"),
            Err(SendError::BadHex) | Err(SendError::Sign)
        ));
    }

    #[test]
    fn rpc_url_gate_requires_https() {
        // Pure check: no env mutation, so no cross-test flakiness.
        assert_eq!(
            ensure_https_rpc("https://sepolia.example/v3/key").unwrap(),
            "https://sepolia.example/v3/key"
        );
        for bad in [
            "http://sepolia.example/v3/key", // plaintext key + tx
            "wss://sepolia.example/ws",
            "sepolia.example/v3/key",
            "",
        ] {
            assert!(
                matches!(ensure_https_rpc(bad), Err(SendError::NonHttpsRpcUrl)),
                "{bad}"
            );
        }
    }
}
