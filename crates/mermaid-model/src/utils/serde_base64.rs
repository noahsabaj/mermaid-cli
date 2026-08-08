//! Serde adapter: `Vec<u8>` ↔ base64 string.
//!
//! Raw byte payloads (pasted images, tool artifacts) ride inside recorded
//! `Msg` JSON for `--record` / `--replay`. Serde's default `Vec<u8>`
//! representation is an array of numbers — ~4x the size and unreadable in a
//! JSONL line; base64 keeps recordings compact while still round-tripping
//! bit-exactly.
//!
//! Use with `#[serde(with = "crate::utils::serde_base64")]` on a `Vec<u8>`
//! field.

use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Deserializer, Serializer};

/// # Errors
///
/// Only what the serializer reports while writing the string; encoding itself
/// is infallible.
pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&general_purpose::STANDARD.encode(bytes))
}

/// # Errors
///
/// When the field is not a string, and when that string is not valid standard
/// base64 — a recording hand-edited into an undecodable payload fails the load
/// rather than yielding empty bytes.
pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(de)?;
    general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(serde::de::Error::custom)
}

/// Base64 serde adapter for opaque UTF-8 strings. The wire value is encoded
/// before persistence redaction sees it, then validated as UTF-8 while loading.
pub mod string {
    use base64::{Engine as _, engine::general_purpose};
    use serde::{Deserialize, Deserializer, Serializer};

    /// # Errors
    ///
    /// Only what the serializer reports while writing the string; encoding
    /// itself is infallible.
    pub fn serialize<S: Serializer>(value: &str, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&general_purpose::STANDARD.encode(value.as_bytes()))
    }

    /// # Errors
    ///
    /// When the field is not a string, when that string is not valid standard
    /// base64, and when the decoded bytes are not UTF-8. The last check is why
    /// this module exists separately: the value is opaque on the wire, so
    /// UTF-8 is only established here, at load.
    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<String, D::Error> {
        let encoded = String::deserialize(de)?;
        let bytes = general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)?;
        String::from_utf8(bytes).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Blob {
        #[serde(with = "super")]
        data: Vec<u8>,
    }

    #[test]
    fn round_trips_bytes_as_base64_string() {
        let blob = Blob {
            data: vec![0, 1, 2, 250, 255],
        };
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(json, r#"{"data":"AAEC+v8="}"#);
        let back: Blob = serde_json::from_str(&json).unwrap();
        assert_eq!(back, blob);
    }

    #[test]
    fn rejects_invalid_base64() {
        let err = serde_json::from_str::<Blob>(r#"{"data":"not base64!!"}"#);
        assert!(err.is_err());
    }

    #[test]
    fn opaque_string_round_trips_and_rejects_non_utf8() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Opaque {
            #[serde(with = "super::string")]
            value: String,
        }
        let value = Opaque {
            value: "eyJopaque.payload.signature".to_string(),
        };
        let json = serde_json::to_string(&value).unwrap();
        assert!(!json.contains("eyJopaque.payload.signature"));
        assert_eq!(serde_json::from_str::<Opaque>(&json).unwrap(), value);
        assert!(serde_json::from_str::<Opaque>(r#"{"value":"/w=="}"#).is_err());
    }
}
