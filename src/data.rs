//! Clipboard data types.

/// Data stored in or retrieved from the clipboard.
#[derive(Debug, Clone)]
pub struct ClipboardData {
    /// The MIME type of the data.
    pub mime_type: String,
    /// The raw data bytes.
    pub data: Vec<u8>,
}

impl ClipboardData {
    /// Create new clipboard data with the given MIME type and raw bytes.
    pub fn new(mime_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self { mime_type: mime_type.into(), data: data.into() }
    }

    /// Create clipboard data from a text string.
    ///
    /// Uses `text/plain;charset=utf-8` as the MIME type.
    pub fn from_text(text: impl AsRef<str>) -> Self {
        Self {
            mime_type: crate::mime::text::PLAIN_UTF8.into(),
            data: text.as_ref().as_bytes().to_vec(),
        }
    }

    /// Try to interpret the data as UTF-8 text.
    ///
    /// Returns `None` if the data is not valid UTF-8.
    pub fn as_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.data).ok()
    }

    /// Convert the data to a String, replacing invalid UTF-8 sequences.
    pub fn to_text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    /// Check if this data represents text content.
    pub fn is_text(&self) -> bool {
        crate::mime::is_text_mime(&self.mime_type)
    }
}

impl From<String> for ClipboardData {
    fn from(text: String) -> Self {
        Self::from_text(text)
    }
}

impl From<&str> for ClipboardData {
    fn from(text: &str) -> Self {
        Self::from_text(text)
    }
}
