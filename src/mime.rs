//! MIME type handling for clipboard operations.

/// Common text MIME types for clipboard operations.
pub mod text {
    /// UTF-8 encoded plain text (primary MIME type).
    pub const PLAIN_UTF8: &str = "text/plain;charset=utf-8";
    /// Plain text without charset specification.
    pub const PLAIN: &str = "text/plain";
    /// X11 compatibility string type.
    pub const UTF8_STRING: &str = "UTF8_STRING";
    /// HTML content.
    pub const HTML: &str = "text/html";
    /// Rich Text Format.
    pub const RTF: &str = "text/rtf";
    /// URI list (for file transfers).
    pub const URI_LIST: &str = "text/uri-list";
}

/// Common image MIME types.
pub mod image {
    /// PNG image.
    pub const PNG: &str = "image/png";
    /// JPEG image.
    pub const JPEG: &str = "image/jpeg";
    /// GIF image.
    pub const GIF: &str = "image/gif";
    /// BMP image.
    pub const BMP: &str = "image/bmp";
    /// WebP image.
    pub const WEBP: &str = "image/webp";
    /// SVG image.
    pub const SVG: &str = "image/svg+xml";
}

/// Application-specific MIME types.
pub mod application {
    /// RTF via application type.
    pub const RTF: &str = "application/rtf";
    /// Generic binary data.
    pub const OCTET_STREAM: &str = "application/octet-stream";
}

/// Default text MIME types used when storing plain text.
///
/// These are offered in order of preference when storing text to the clipboard.
pub static TEXT_MIME_TYPES: [&str; 3] = [text::PLAIN_UTF8, text::UTF8_STRING, text::PLAIN];

/// Find the first matching MIME type from the offered types.
///
/// Returns the first MIME type from `preferred` that exists in `offered`, or `None`
/// if no match is found.
pub fn find_preferred_mime<'a>(offered: &[String], preferred: &[&'a str]) -> Option<&'a str> {
    for pref in preferred {
        if offered.iter().any(|o| o == *pref) {
            return Some(pref);
        }
    }
    None
}

/// Check if a MIME type represents text content.
pub fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/") || mime == text::UTF8_STRING
}

/// Normalize CR and CRLF into LF.
///
/// 'text' mime types require CRLF line ending according to
/// RFC-2046, however the platform line terminator and what applications
/// expect is LF.
pub fn normalize_to_lf(text: String) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
