//! Solana DEVNET send probe over HTTPS JSON-RPC (`SOLANA_RPC_URL` only).
//!
//! Jupiter is mainnet-only, so this probe skips swaps entirely: a tiny SOL
//! self-transfer proving RPC + signing + send on devnet.
//!
//! FAIL-CLOSED: `getGenesisHash` must equal [`DEVNET_GENESIS_HASH`] before
//! anything is signed or sent. Secrets (the RPC URL may embed an API key;
//! `key_base58` is a raw private key) never appear in errors or logs.
//!
//! Hash provenance: `getGenesisHash` against `https://api.devnet.solana.com`
//! returned this hash on two live queries (2026-10-27); the same session's
//! mainnet query returned the well-known `5eykt4...9d`, validating the
//! method. The `getGenesisHash` docs page shows a different hash in its
//! example output, but that is illustrative, not live devnet data.
//! <https://solana.com/docs/rpc/http/getgenesishash>

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::Value;
use std::time::Duration;

/// Devnet genesis hash. Any other `getGenesisHash` aborts the send.
pub const DEVNET_GENESIS_HASH: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
/// Mainnet genesis hash — refused here; the gate is chain-specific.
pub const MAINNET_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
/// System program (`1111...` = 32 zero bytes) owns every SOL transfer.
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("SOLANA_RPC_URL is unset or empty; refusing to send")]
    MissingRpcUrl,
    #[error("refusing send: SOLANA_RPC_URL must be https")]
    NonHttpsRpcUrl,
    #[error("refusing send: RPC is not on devnet genesis")]
    WrongGenesis,
    #[error("bad secret key or base58 value")]
    BadKey,
    #[error("RPC method {method} failed: {detail}")]
    Rpc {
        method: &'static str,
        detail: String,
    },
    #[error("transport error: {0}")]
    Transport(String),
}

/// Endpoint for every send path. ONLY `SOLANA_RPC_URL` — never a literal, arg, or WSS URL.
pub fn rpc_url_from_env() -> Result<String, SendError> {
    let url = std::env::var("SOLANA_RPC_URL")
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

/// Fail-closed genesis gate: devnet or nothing.
pub fn ensure_devnet_genesis(hash: &str) -> Result<(), SendError> {
    if hash == DEVNET_GENESIS_HASH {
        Ok(())
    } else {
        Err(SendError::WrongGenesis)
    }
}

fn b58(s: &str) -> Result<Vec<u8>, SendError> {
    bs58::decode(s).into_vec().map_err(|_| SendError::BadKey)
}

/// Seed from a base58 secret key: a 64-byte `solana-keygen` export
/// (seed + pubkey) or a bare 32-byte seed.
pub fn seed_from_base58(secret: &str) -> Result<[u8; 32], SendError> {
    let raw = b58(secret)?;
    if raw.len() == 32 {
        Ok(raw.try_into().expect("len checked"))
    } else if raw.len() == 64 {
        Ok(raw[..32].try_into().expect("len checked"))
    } else {
        Err(SendError::BadKey)
    }
}

/// ed25519 pubkey bytes for a seed (the account address decodes from these).
pub fn pubkey_of(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Solana short-vec (compact-u16) length prefix.
fn compact_u16(mut v: usize, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Legacy self-transfer message: one signature, accounts `[self, system]`,
/// one SystemProgram transfer instruction (discriminator 2 + LE lamports).
pub fn self_transfer_message(from: &[u8; 32], blockhash: &[u8; 32], lamports: u64) -> Vec<u8> {
    let mut m = vec![1u8, 0, 1]; // 1 sig required, 0 readonly-signed, 1 readonly-unsigned
    compact_u16(2, &mut m);
    m.extend_from_slice(from);
    m.extend_from_slice(&[0u8; 32]); // system program
    m.extend_from_slice(blockhash);
    compact_u16(1, &mut m); // one instruction
    m.push(1); // program id index -> system program
    compact_u16(2, &mut m);
    m.push(0);
    m.push(0); // from = to = accounts[0]
    compact_u16(12, &mut m);
    m.extend_from_slice(&[2, 0, 0, 0]); // SystemInstruction::Transfer
    m.extend_from_slice(&lamports.to_le_bytes());
    compact_u16(0, &mut m); // no address-table lookups
    m
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

fn parse_blockhash(v: &Value) -> Result<[u8; 32], SendError> {
    const M: &str = "getLatestBlockhash";
    let bad = |detail: &str| SendError::Rpc {
        method: M,
        detail: detail.into(),
    };
    let s = v
        .get("value")
        .and_then(|x| x.get("blockhash"))
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing value.blockhash"))?;
    bs58::decode(s)
        .into_vec()
        .map_err(|_| bad("bad base58 blockhash"))?
        .try_into()
        .map_err(|_| bad("blockhash is not 32 bytes"))
}

/// Send a tiny SOL self-transfer on devnet; returns the base58 signature.
///
/// Reads the endpoint ONLY from `SOLANA_RPC_URL`. Fails closed unless
/// `getGenesisHash` is devnet; nothing is signed before the gate passes.
pub async fn send_devnet_self_transfer(
    key_base58: &str,
    lamports: u64,
) -> Result<String, SendError> {
    let rpc_url = rpc_url_from_env()?;
    // Same explicit ring provider as arb-exec's sender.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| SendError::Transport(e.without_url().to_string()))?;
    let genesis = rpc_call(&client, &rpc_url, "getGenesisHash", Value::Array(vec![])).await?;
    let genesis = genesis.as_str().ok_or(SendError::Rpc {
        method: "getGenesisHash",
        detail: "non-string hash".into(),
    })?;
    ensure_devnet_genesis(genesis)?;
    let seed = seed_from_base58(key_base58)?;
    let from = pubkey_of(&seed);
    let blockhash = parse_blockhash(
        &rpc_call(
            &client,
            &rpc_url,
            "getLatestBlockhash",
            Value::Array(vec![]),
        )
        .await?,
    )?;
    let msg = self_transfer_message(&from, &blockhash, lamports);
    let sig = SigningKey::from_bytes(&seed).sign(&msg);
    let mut wire = Vec::with_capacity(64 + msg.len());
    wire.extend_from_slice(&sig.to_bytes());
    wire.extend_from_slice(&msg);
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&wire);
    let out = rpc_call(
        &client,
        &rpc_url,
        "sendTransaction",
        serde_json::json!([tx_b64, {"encoding": "base64"}]),
    )
    .await?;
    out.as_str().map(str::to_string).ok_or(SendError::Rpc {
        method: "sendTransaction",
        detail: "non-string signature".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier as _;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn genesis_gate_only_accepts_devnet() {
        assert!(ensure_devnet_genesis(DEVNET_GENESIS_HASH).is_ok());
        assert_eq!(DEVNET_GENESIS_HASH.len(), 44);
        for bad in [
            MAINNET_GENESIS_HASH, // right network family, wrong cluster
            "GH7ome3EiwEr7tu9JuTh2dpYWBJK3z69Xm1ZE3MEE6JC", // docs-page example output, not live devnet
            "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBH", // last-char typo
            "",
        ] {
            assert!(
                matches!(ensure_devnet_genesis(bad), Err(SendError::WrongGenesis)),
                "{bad}"
            );
        }
    }

    #[test]
    fn rpc_url_missing_or_empty_refuses() {
        let saved = std::env::var("SOLANA_RPC_URL").ok();
        std::env::remove_var("SOLANA_RPC_URL");
        assert!(matches!(rpc_url_from_env(), Err(SendError::MissingRpcUrl)));
        std::env::set_var("SOLANA_RPC_URL", "   ");
        assert!(matches!(rpc_url_from_env(), Err(SendError::MissingRpcUrl)));
        match saved {
            Some(v) => std::env::set_var("SOLANA_RPC_URL", v),
            None => std::env::remove_var("SOLANA_RPC_URL"),
        }
    }

    #[test]
    fn system_program_is_32_zero_bytes() {
        assert_eq!(b58(SYSTEM_PROGRAM).unwrap(), vec![0u8; 32]);
    }

    #[test]
    fn compact_u16_vectors() {
        let mut out = Vec::new();
        for v in [0usize, 1, 127, 128, 300, 16383] {
            compact_u16(v, &mut out);
        }
        assert_eq!(out, [0x00, 0x01, 0x7f, 0x80, 0x01, 0xac, 0x02, 0xff, 0x7f]);
    }

    // RFC 8032 test vector 1 (seed -> pubkey): the signing oracle.
    const RFC_SEED_HEX: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const RFC_PUB_HEX: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    fn rfc_seed() -> [u8; 32] {
        unhex(RFC_SEED_HEX).try_into().unwrap()
    }

    #[test]
    fn rfc8032_seed_gives_known_pubkey() {
        assert_eq!(pubkey_of(&rfc_seed()), unhex(RFC_PUB_HEX).as_slice());
    }

    #[test]
    fn self_transfer_message_layout_golden() {
        let from = [1u8; 32];
        let blockhash = [2u8; 32];
        let m = self_transfer_message(&from, &blockhash, 1);
        assert_eq!(m.len(), 119); // 3 + 65 + 32 + 1 + 17 + 1
        assert_eq!(&m[..3], &[1, 0, 1]);
        assert_eq!(&m[3..4], &[0x02]);
        assert_eq!(&m[4..36], &[1u8; 32]);
        assert_eq!(&m[36..68], &[0u8; 32]); // system program
        assert_eq!(&m[68..100], &[2u8; 32]);
        assert_eq!(&m[100..102], &[0x01, 0x01]); // 1 ix, program id index 1
        assert_eq!(&m[102..105], &[0x02, 0x00, 0x00]); // accounts [0, 0]
        assert_eq!(&m[105..106], &[0x0c]); // 12 data bytes follow
        assert_eq!(&m[106..110], &[2, 0, 0, 0]); // Transfer discriminator
        assert_eq!(&m[110..118], &[1, 0, 0, 0, 0, 0, 0, 0]); // 1 lamport LE
        assert_eq!(&m[118..119], &[0x00]); // no lookups
    }

    #[test]
    fn sign_verifies_and_is_deterministic() {
        let seed = rfc_seed();
        let sk = SigningKey::from_bytes(&seed);
        let msg = self_transfer_message(&pubkey_of(&seed), &[3u8; 32], 1);
        let sig = sk.sign(&msg);
        assert!(sk.verifying_key().verify(&msg, &sig).is_ok());
        assert_eq!(sig.to_bytes(), sk.sign(&msg).to_bytes());
        let mut bad = msg.clone();
        bad[10] ^= 1;
        assert!(sk.verifying_key().verify(&bad, &sig).is_err());
    }

    #[test]
    fn seed_shapes_32_or_64_bytes_only() {
        let seed32 = bs58::encode([5u8; 32]).into_string();
        assert_eq!(seed_from_base58(&seed32).unwrap(), [5u8; 32]);
        let mut export = [0u8; 64];
        export[..32].copy_from_slice(&rfc_seed());
        export[32..].copy_from_slice(&unhex(RFC_PUB_HEX));
        assert_eq!(
            seed_from_base58(&bs58::encode(&export).into_string()).unwrap(),
            rfc_seed()
        );
        assert!(matches!(seed_from_base58("!!!"), Err(SendError::BadKey)));
        assert!(matches!(
            seed_from_base58(&bs58::encode([0u8; 16]).into_string()),
            Err(SendError::BadKey)
        ));
    }

    #[test]
    fn rpc_url_gate_requires_https() {
        // Pure check: no env mutation, so no cross-test flakiness.
        assert_eq!(
            ensure_https_rpc("https://api.devnet.solana.com").unwrap(),
            "https://api.devnet.solana.com"
        );
        for bad in [
            "http://api.devnet.solana.com", // plaintext key + tx
            "wss://api.devnet.solana.com",
            "api.devnet.solana.com",
            "",
        ] {
            assert!(
                matches!(ensure_https_rpc(bad), Err(SendError::NonHttpsRpcUrl)),
                "{bad}"
            );
        }
    }

    #[test]
    fn blockhash_parses_nested_response_shape() {
        let good: Value = serde_json::json!({
            "value": { "blockhash": bs58::encode([9u8; 32]).into_string(), "lastValidBlockHeight": 1 }
        });
        assert_eq!(parse_blockhash(&good).unwrap(), [9u8; 32]);
        assert!(parse_blockhash(&serde_json::json!({})).is_err());
        assert!(parse_blockhash(&serde_json::json!({"value": {"blockhash": "!!!"}})).is_err());
        assert!(parse_blockhash(
            &serde_json::json!({"value": {"blockhash": bs58::encode([0u8; 16]).into_string()}})
        )
        .is_err());
    }
}
