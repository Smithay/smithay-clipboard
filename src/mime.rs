static ALLOWED_MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "UTF8_STRING"];

/// Mime type supported by clipboard.
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum MimeType {
    /// text/plain;charset=utf-8 mime type.
    TextPlainUtf8 = 0,
    /// UTF8_STRING mime type.
    Utf8String = 1,
}

impl MimeType {
    /// Find first allowed mime type among the `offered_mime_types`.
    ///
    /// `find_allowed()` searches for mime type clipboard supports, if we have a match,
    /// returns `Some(MimeType)`, otherwise `None`.
    pub fn find_allowed(offered_mime_types: &[String]) -> Option<Self> {
        for offered_mime_type in offered_mime_types.into_iter() {
            if offered_mime_type == ALLOWED_MIME_TYPES[Self::TextPlainUtf8 as usize] {
                return Some(Self::TextPlainUtf8);
            } else if offered_mime_type == ALLOWED_MIME_TYPES[Self::Utf8String as usize] {
                return Some(Self::Utf8String);
            }
        }

        None
    }
}

impl ToString for MimeType {
    fn to_string(&self) -> String {
        String::from(ALLOWED_MIME_TYPES[*self as usize])
    }
}
