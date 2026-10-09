//! Image formats, by their bytes rather than a file name.
//!
//! Images travel through Mermaid as bare base64 strings, so the wire
//! adapters sniff the media type from the first bytes. PNG, JPEG, GIF and
//! WebP are the four every vision provider accepts.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The media type of an image, from its first bytes; `None` when they are
/// not one of the four formats every vision provider accepts.
#[must_use]
pub fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The media type of a base64-encoded image. Clipboard pastes are PNG, so
/// anything unrecognised is sent as PNG and the provider's error names it.
#[must_use]
pub fn base64_image_media_type(data: &str) -> &'static str {
    // 16 base64 characters decode to the 12 bytes the longest signature needs.
    let head = data.get(..16).unwrap_or(data);
    STANDARD
        .decode(head)
        .ok()
        .and_then(|bytes| image_media_type(&bytes))
        .unwrap_or("image/png")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_format_is_known_by_its_signature() {
        assert_eq!(
            image_media_type(b"\x89PNG\r\n\x1a\n...."),
            Some("image/png")
        );
        assert_eq!(
            image_media_type(b"\xff\xd8\xff\xe0...."),
            Some("image/jpeg")
        );
        assert_eq!(image_media_type(b"GIF89a......"), Some("image/gif"));
        assert_eq!(
            image_media_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(image_media_type(b"fn main() {}"), None);
        assert_eq!(image_media_type(b""), None);
    }

    #[test]
    fn base64_sniffs_the_decoded_head() {
        let jpeg = STANDARD.encode(b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0");
        assert_eq!(base64_image_media_type(&jpeg), "image/jpeg");
        let webp = STANDARD.encode(b"RIFF\x24\0\0\0WEBPVP8 \0\0");
        assert_eq!(base64_image_media_type(&webp), "image/webp");
    }

    #[test]
    fn base64_falls_back_to_png() {
        assert_eq!(base64_image_media_type("BASE64DATA"), "image/png");
        assert_eq!(base64_image_media_type(""), "image/png");
        assert_eq!(base64_image_media_type("not base64 at all!"), "image/png");
    }
}
