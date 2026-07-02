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

pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&general_purpose::STANDARD.encode(bytes))
}

pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(de)?;
    general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(serde::de::Error::custom)
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
}
