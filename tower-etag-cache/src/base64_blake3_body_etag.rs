//! Function that calculates a `HeaderValue` as the base64-encoded blake3 hash of a byte slice

use data_encoding::BASE64;
use http::HeaderValue;

/// Calculates the etag value as base64 encoded blake3 hash of the body bytes
pub fn base64_blake3_body_etag(body: impl AsRef<[u8]>) -> HeaderValue {
    let mut hasher = blake3::Hasher::new();
    hasher.update(body.as_ref());
    let bytes = hasher.finalize();
    let val = BASE64.encode(bytes.as_bytes());
    // unwrap-safety: base64 should be always valid ascii
    HeaderValue::from_str(&format!(r#""{val}""#)).unwrap()
}
