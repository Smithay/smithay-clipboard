use std::borrow::Cow;
use std::{error, fmt};

/// List of allowed mimes.
pub static ALLOWED_TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

#[derive(Debug, Clone, Copy)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unsupported mime type")
    }
}

impl error::Error for Error {}

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum Text {
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
}

/// Mime type supported by clipboard.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum MimeType {
    /// Text mime type.
    Text(Text),
    /// Other mime type.
    Other(Cow<'static, str>),
}

impl Default for MimeType {
    fn default() -> Self {
        MimeType::Text(Text::default())
    }
}

impl From<Cow<'static, str>> for MimeType {
    fn from(value: Cow<'static, str>) -> Self {
        if let Some(pos) = ALLOWED_TEXT_MIME_TYPES.into_iter().position(|allowed| allowed == value)
        {
            MimeType::Text(match pos {
                0 => Text::TextPlainUtf8,
                1 => Text::Utf8String,
                2 => Text::TextPlain,
                _ => unreachable!(),
            })
        } else {
            MimeType::Other(value)
        }
    }
}

impl AsRef<str> for MimeType {
    fn as_ref(&self) -> &str {
        match self {
            MimeType::Other(s) => s.as_ref(),
            MimeType::Text(text) => ALLOWED_TEXT_MIME_TYPES[*text as usize],
        }
    }
}

/// Describes the mime types which are accepted.
pub trait AllowedMimeTypes: TryFrom<(Vec<u8>, MimeType)> {
    /// List allowed mime types for the type to convert from a byte slice.
    ///
    /// Allowed mime types should be listed in order of decreasing preference,
    /// most preferred first.
    fn allowed() -> Cow<'static, [MimeType]>;
}

/// Can be converted to data with the available mime types.
pub trait AsMimeTypes {
    /// List available mime types for this data to convert to a byte slice.
    fn available(&self) -> Cow<'static, [MimeType]>;

    /// Converts a type to a byte slice for the given mime type if possible.
    fn as_bytes(&self, mime_type: &MimeType) -> Option<Cow<'static, [u8]>>;
}

impl MimeType {
    /// Find first offered mime type among the `allowed_mime_types`.
    ///
    /// `find_allowed()` searches for mime type clipboard supports, if we have a
    /// match, returns `Some(MimeType)`, otherwise `None`.
    pub(crate) fn find_allowed(
        offered_mime_types: &[String],
        allowed_mime_types: &[Self],
    ) -> Option<Self> {
        allowed_mime_types
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
            MimeType::Text(text) => write!(f, "{}", ALLOWED_TEXT_MIME_TYPES[*text as usize]),
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use crate::mime::{MimeType, ALLOWED_TEXT_MIME_TYPES};

    #[test]
    fn test_from_str() {
        assert_eq!(
            MimeType::from(Cow::Borrowed(ALLOWED_TEXT_MIME_TYPES[0])),
            MimeType::Text(crate::mime::Text::TextPlainUtf8)
        );
        assert_eq!(
            MimeType::from(Cow::Borrowed(ALLOWED_TEXT_MIME_TYPES[1])),
            MimeType::Text(crate::mime::Text::Utf8String)
        );
        assert_eq!(
            MimeType::from(Cow::Borrowed(ALLOWED_TEXT_MIME_TYPES[2])),
            MimeType::Text(crate::mime::Text::TextPlain)
        );
    }
}
