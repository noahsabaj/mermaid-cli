/// Attachment state for image paste support
///
/// Manages pending image attachments before they are sent with a message.

use base64::{engine::general_purpose, Engine as _};
use std::path::PathBuf;

/// A single image attachment ready to send
pub struct ImageAttachment {
    /// Base64-encoded image data for the Ollama API
    pub base64_data: String,
    /// Temp file path for preview with xdg-open
    pub temp_path: PathBuf,
    /// Original image size in bytes
    pub size_bytes: usize,
    /// Image format: "png" or "jpeg"
    pub format: String,
}

/// State for pending image attachments
pub struct AttachmentState {
    pub attachments: Vec<ImageAttachment>,
    next_id: usize,
}

impl AttachmentState {
    pub fn new() -> Self {
        Self {
            attachments: Vec::new(),
            next_id: 1,
        }
    }

    /// Add an image from raw bytes. Returns the attachment number.
    pub fn add(&mut self, image_bytes: &[u8], format: String) -> usize {
        let id = self.next_id;
        self.next_id += 1;

        let size_bytes = image_bytes.len();
        let base64_data = general_purpose::STANDARD.encode(image_bytes);

        // Write temp file for preview
        let temp_path = PathBuf::from(format!("/tmp/mermaid-img-{}.{}", id, format));
        let _ = std::fs::write(&temp_path, image_bytes);

        self.attachments.push(ImageAttachment {
            base64_data,
            temp_path,
            size_bytes,
            format,
        });

        id
    }

    /// Remove the last attachment. Returns true if one was removed.
    pub fn remove_last(&mut self) -> bool {
        if let Some(attachment) = self.attachments.pop() {
            let _ = std::fs::remove_file(&attachment.temp_path);
            true
        } else {
            false
        }
    }

    /// Remove attachment at a specific index. Returns true if one was removed.
    pub fn remove_at(&mut self, index: usize) -> bool {
        if index < self.attachments.len() {
            let attachment = self.attachments.remove(index);
            let _ = std::fs::remove_file(&attachment.temp_path);
            true
        } else {
            false
        }
    }

    /// Clear all attachments and clean up temp files.
    pub fn clear(&mut self) {
        for attachment in self.attachments.drain(..) {
            let _ = std::fs::remove_file(&attachment.temp_path);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty()
    }

    pub fn len(&self) -> usize {
        self.attachments.len()
    }

    /// Take all base64 image data for the Ollama API, clearing attachments.
    /// Returns None if no attachments.
    pub fn take_base64_data(&mut self) -> Option<Vec<String>> {
        if self.attachments.is_empty() {
            return None;
        }
        let data: Vec<String> = self.attachments.iter().map(|a| a.base64_data.clone()).collect();
        self.clear();
        Some(data)
    }

    /// Get the temp path of the last attachment (for preview).
    pub fn last_temp_path(&self) -> Option<&PathBuf> {
        self.attachments.last().map(|a| &a.temp_path)
    }
}

impl Drop for AttachmentState {
    fn drop(&mut self) {
        self.clear();
    }
}
