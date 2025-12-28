//! Error types for clipboard operations.

use thiserror::Error;

/// The error type for clipboard operations.
#[derive(Debug, Error)]
pub enum ClipboardError {
    /// No events have been received on any seat yet.
    #[error("no events received on any seat")]
    NoSeat,

    /// The client doesn't have keyboard focus.
    #[error("client doesn't have keyboard focus")]
    NoFocus,

    /// The clipboard selection is empty.
    #[error("clipboard selection is empty")]
    Empty,

    /// The requested MIME type is not available in the clipboard.
    #[error("requested MIME type not available: {0}")]
    MimeNotAvailable(String),

    /// No compatible MIME type found among the offered types.
    #[error("no compatible MIME type found")]
    NoCompatibleMime,

    /// The clipboard data is not valid UTF-8.
    #[error("clipboard data is not valid UTF-8")]
    InvalidUtf8,

    /// The primary selection protocol is not supported by the compositor.
    #[error("primary selection is not supported")]
    PrimarySelectionUnsupported,

    /// The data device protocol is not supported by the compositor.
    #[error("data device is not supported")]
    DataDeviceUnsupported,

    /// The clipboard worker thread has died.
    #[error("clipboard worker thread has died")]
    WorkerDead,

    /// An I/O error occurred during clipboard operation.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A specialized `Result` type for clipboard operations.
pub type Result<T> = std::result::Result<T, ClipboardError>;
