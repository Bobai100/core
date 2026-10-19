//! Constant-time HMAC-SHA256 verification for inbound KYC webhook authentication.
//! The comparison is delegated to [`Mac::verify_slice`], which is constant-time,
//! so its duration does not depend on the position of the first mismatched byte.

use anyhow::{anyhow, Result};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Verifies an inbound KYC webhook payload using constant-time HMAC-SHA256 comparison.
pub fn verify_kyc_webhook(
    payload_body: &[u8],
    secret_key: &str,
    received_signature_hex: &str,
) -> Result<bool> {
    let clean_hex = received_signature_hex
        .strip_prefix("sha256=")
        .unwrap_or(received_signature_hex)
        .trim();

    let expected_signature_bytes =
        hex::decode(clean_hex).map_err(|e| anyhow!("invalid hex signature: {}", e))?;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())
        .map_err(|e| anyhow!("invalid HMAC key: {}", e))?;
    mac.update(payload_body);
    Ok(mac.verify_slice(&expected_signature_bytes).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_kyc_webhook_valid_and_invalid() {
        let secret = "kyc-webhook-secret-token";
        let payload = br#"{"user_id":"usr_123","status":"VERIFIED"}"#;

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let valid_hex = hex::encode(mac.finalize().into_bytes());

        // Valid signature passes
        assert!(verify_kyc_webhook(payload, secret, &valid_hex).unwrap());
        assert!(verify_kyc_webhook(payload, secret, &format!("sha256={}", valid_hex)).unwrap());

        // Tampered signature fails
        let tampered_hex = format!("00{}", &valid_hex[2..]);
        assert!(!verify_kyc_webhook(payload, secret, &tampered_hex).unwrap());

        // Truncated signature fails
        assert!(!verify_kyc_webhook(payload, secret, &valid_hex[..62]).unwrap());

        // Tampered payload fails
        assert!(!verify_kyc_webhook(b"tampered", secret, &valid_hex).unwrap());
    }
}
