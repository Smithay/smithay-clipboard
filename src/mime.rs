use std::borrow::Cow;
use thiserror::Error;

/// List of allowed mimes.
pub static ALLOWED_TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

#[derive(Error, Debug)]
pub enum Error {
    #[error("Unsupported mime type")]
    Unsupported,
}

/// Mime type supported by clipboard.
#[derive(Clone, Eq, PartialEq, Debug, Default)]
#[repr(u8)]
pub enum MimeType {
    #[default]
    /// text/plain;charset=utf-8 mime type.
    ///
    /// The primary mime type used by most clients
    TextPlainUtf8 = 0,
    /// UTF8_STRING mime type.
    ///
    /// Some X11 clients are using only this mime type, so we
    /// should have it as a fallback just in case.
    Utf8String = 1,
    /// text/plain mime type.
    ///
    /// Fallback without charset parameter.
    TextPlain = 2,
    /// Other mime type
    Other(Cow<'static, str>),
}

impl AsRef<str> for MimeType {
    fn as_ref(&self) -> &str {
        match self {
            MimeType::Other(s) => s.as_ref(),
            m => ALLOWED_TEXT_MIME_TYPES[m.discriminant() as usize],
        }
    }
}

impl MimeType {
    fn discriminant(&self) -> u8 {
        unsafe { *(self as *const Self as *const u8) }
    }
}

/// Describes the mime types which are accepted
pub trait AllowedMimeTypes: TryFrom<(Vec<u8>, MimeType)> {
    fn allowed() -> Cow<'static, [MimeType]>;
}

/// Can be converted to data with the available mime types
pub trait AsMimeTypes {
    /// Available mime types for this data
    fn available(&self) -> Cow<'static, [MimeType]>;

    /// Data as a specific mime_type
    fn as_bytes(&self, mime_type: &MimeType) -> Option<Cow<'static, [u8]>>;
}

impl MimeType {
    /// Find first allowed mime type among the `offered_mime_types`.
    ///
    /// `find_allowed()` searches for mime type clipboard supports, if we have a
    /// match, returns `Some(MimeType)`, otherwise `None`.
    pub fn find_allowed(offered_mime_types: &[String], allowed: &[Self]) -> Option<Self> {
        allowed
            .iter()
            .find(|allowed| {
                offered_mime_types.iter().any(|offered| offered.as_str() == allowed.as_ref())
            })
            .cloned()
    }
}

impl std::fmt::Display for MimeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MimeType::Other(m) => write!(f, "{}", m),
            m => write!(f, "{}", ALLOWED_TEXT_MIME_TYPES[m.discriminant() as usize]),
        }
    }
}

/// Normalize CR and CRLF into LF.
///
/// 'text' mime types require CRLF line ending according to
/// RFC-2046, however the platform line terminator and what applications
/// expect is LF.
pub fn normalize_to_lf(text: String) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
