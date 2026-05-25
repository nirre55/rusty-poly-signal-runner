use alloy::primitives::{Address, B256, U256};
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine;
use rusty_poly_signal_runner::polymarket::PolymarketClient;
use std::str::FromStr;

#[test]
fn test_build_slug() {
    // 1710000000000 ms → 1710000000 s
    let slug = PolymarketClient::build_slug("btc-updown-5m", 1710000000000);
    assert_eq!(slug, "btc-updown-5m-1710000000");
}

#[test]
fn test_ctf_domain_separator_is_deterministic() {
    let sep1 = PolymarketClient::ctf_domain_separator().unwrap();
    let sep2 = PolymarketClient::ctf_domain_separator().unwrap();
    assert_eq!(sep1, sep2);
    assert_ne!(sep1, [0u8; 32], "domain separator ne doit pas être zéro");
}

#[test]
fn test_clob_auth_domain_separator_is_deterministic() {
    let sep1 = PolymarketClient::clob_auth_domain_separator();
    let sep2 = PolymarketClient::clob_auth_domain_separator();
    assert_eq!(sep1, sep2);
    assert_ne!(sep1, [0u8; 32]);
}

#[test]
fn test_order_signing_hash_is_deterministic() {
    let salt = U256::from(12345u64);
    let maker = Address::from_str("0x0000000000000000000000000000000000000001").unwrap();
    let token_id = U256::from(9999u64);
    let h1 = PolymarketClient::order_signing_hash(
        salt,
        maker,
        token_id,
        U256::from(10_000_000u64),
        U256::from(1u64),
        U256::ZERO,
        0,
        0,
    )
    .unwrap();
    let h2 = PolymarketClient::order_signing_hash(
        salt,
        maker,
        token_id,
        U256::from(10_000_000u64),
        U256::from(1u64),
        U256::ZERO,
        0,
        0,
    )
    .unwrap();
    assert_eq!(h1, h2);
    assert_ne!(h1, B256::ZERO);
}

#[test]
fn test_clob_auth_signing_hash_is_deterministic() {
    let addr = Address::from_str("0x0000000000000000000000000000000000000001").unwrap();
    let h1 = PolymarketClient::clob_auth_signing_hash(addr, "1742256000", 0).unwrap();
    let h2 = PolymarketClient::clob_auth_signing_hash(addr, "1742256000", 0).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn test_hmac_sig_wrong_secret_fails() {
    // Un secret non-base64url doit retourner une erreur
    let result =
        PolymarketClient::compute_hmac_sig("not!valid!base64", "123", "POST", "/order", "{}");
    assert!(result.is_err());
}

#[test]
fn test_hmac_sig_valid_secret() {
    // Secret base64url valide (32 zéros encodés)
    let secret = URL_SAFE.encode([0u8; 32]);
    let result = PolymarketClient::compute_hmac_sig(
        &secret,
        "1742256000",
        "POST",
        "/order",
        r#"{"test":1}"#,
    );
    assert!(result.is_ok());
    let sig = result.unwrap();
    assert!(!sig.is_empty());
}
