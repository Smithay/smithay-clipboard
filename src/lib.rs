//! Smithay Clipboard
//!
//! Provides access to the Wayland clipboard for gui applications. The user
//! should have surface around.
//!
//! # Examples
//!
//! ## Store and load text
//!
//! ```no_run
//! # fn main() -> smithay_clipboard::Result<()> {
//! # let display: *mut std::ffi::c_void = std::ptr::null_mut();
//! let clipboard = unsafe { smithay_clipboard::Clipboard::new(display) };
//!
//! // Store text to clipboard
//! clipboard.store_text("Hello, World!");
//!
//! // Load text from clipboard
//! let text = clipboard.load_text()?;
//! println!("Clipboard contains: {}", text);
//! # Ok(())
//! # }
//! ```
//!
//! ## Store and load images
//!
//! ```no_run
//! # fn main() -> smithay_clipboard::Result<()> {
//! # let display: *mut std::ffi::c_void = std::ptr::null_mut();
//! use smithay_clipboard::mime;
//!
//! let clipboard = unsafe { smithay_clipboard::Clipboard::new(display) };
//!
//! // Store PNG image data
//! let png_data: Vec<u8> = vec![/* PNG bytes */];
//! clipboard.store(&png_data, &[mime::image::PNG]);
//!
//! // Load image data (trying PNG first, then JPEG)
//! let image = clipboard.load(&[mime::image::PNG, mime::image::JPEG])?;
//! println!("Got image with MIME type: {}", image.mime_type);
//! # Ok(())
//! # }
//! ```

#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver};

use sctk::reexports::calloop::channel::{self, Sender};
use sctk::reexports::client::Connection;
use sctk::reexports::client::backend::Backend;

mod data;
#[cfg(feature = "dnd")]
pub mod dnd;
pub mod error;
pub mod mime;
mod state;
mod worker;

pub use data::ClipboardData;
pub use error::{ClipboardError, Result};

use worker::{Command, Reply};

#[cfg(feature = "dnd")]
use worker::DndCommand;

/// Access to a Wayland clipboard.
pub struct Clipboard {
    request_sender: Sender<Command>,
    request_receiver: Receiver<Result<Reply>>,
    clipboard_thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(feature = "dnd")]
    #[allow(dead_code)]
    connection: Connection,
}

impl Clipboard {
    /// Creates new clipboard which will be running on its own thread with its
    /// own event queue to handle clipboard requests.
    ///
    /// # Safety
    ///
    /// `display` must be a valid `*mut wl_display` pointer, and it must remain
    /// valid for as long as `Clipboard` object is alive.
    pub unsafe fn new(display: *mut c_void) -> Self {
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);

        // Create channel to send data to clipboard thread.
        let (request_sender, rx_chan) = channel::channel();
        // Create channel to get data from the clipboard thread.
        let (clipboard_reply_sender, request_receiver) = mpsc::channel();

        let name = String::from("smithay-clipboard");
        let clipboard_thread = worker::spawn(name, connection.clone(), rx_chan, clipboard_reply_sender);

        Self {
            request_receiver,
            request_sender,
            clipboard_thread,
            #[cfg(feature = "dnd")]
            connection,
        }
    }
    // ========================================================================
    // Generic API
    // ========================================================================

    /// Load data from clipboard with preferred MIME types.
    ///
    /// The first available MIME type from `mime_types` will be used.
    /// Returns the data along with the actual MIME type used.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let clipboard: smithay_clipboard::Clipboard = todo!();
    /// use smithay_clipboard::mime;
    ///
    /// // Try to load as PNG first, fall back to JPEG
    /// let data = clipboard.load(&[mime::image::PNG, mime::image::JPEG])?;
    /// # Ok::<(), smithay_clipboard::ClipboardError>(())
    /// ```
    pub fn load(&self, mime_types: &[&str]) -> Result<ClipboardData> {
        let mimes: Vec<String> = mime_types.iter().map(|s| s.to_string()).collect();
        let _ = self.request_sender.send(Command::Load { mime_types: mimes });

        match self.request_receiver.recv() {
            Ok(Ok(Reply::Data(data))) => Ok(data),
            Ok(Ok(_)) => Err(ClipboardError::Empty),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ClipboardError::WorkerDead),
        }
    }

    /// Store data to clipboard with specified MIME types.
    ///
    /// The data will be offered to other applications with all the specified
    /// MIME types. Use this when the same data can be represented by multiple
    /// MIME types (e.g., text as both `text/plain` and `UTF8_STRING`).
    ///
    /// For storing different data for different MIME types (e.g., text + image),
    /// use [`store_multi`](Self::store_multi) instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let clipboard: smithay_clipboard::Clipboard = todo!();
    /// use smithay_clipboard::mime;
    ///
    /// let png_data: Vec<u8> = vec![/* PNG bytes */];
    /// clipboard.store(&png_data, &[mime::image::PNG]);
    /// ```
    pub fn store(&self, data: &[u8], mime_types: &[&str]) {
        let request = Command::Store {
            data: data.to_vec(),
            mime_types: mime_types.iter().map(|s| s.to_string()).collect(),
        };
        let _ = self.request_sender.send(request);
    }

    /// Store multiple formats to clipboard with different data per format.
    ///
    /// This allows storing different representations of the same content, so
    /// applications can choose the format they prefer. For example, you can
    /// offer both plain text and HTML, or text and an image.
    ///
    /// Each tuple contains the data and a list of MIME types for that data.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let clipboard: smithay_clipboard::Clipboard = todo!();
    /// use smithay_clipboard::mime;
    ///
    /// // Offer the same content as plain text, HTML, and an image
    /// let text = b"Hello World";
    /// let html = b"<b>Hello World</b>";
    /// let png_data: Vec<u8> = vec![/* PNG bytes */];
    ///
    /// clipboard.store_multi(&[
    ///     (text, &[mime::text::PLAIN_UTF8, mime::text::PLAIN]),
    ///     (html, &[mime::text::HTML]),
    ///     (&png_data, &[mime::image::PNG]),
    /// ]);
    /// ```
    pub fn store_multi(&self, formats: &[(&[u8], &[&str])]) {
        let formats: Vec<(Vec<u8>, Vec<String>)> = formats
            .iter()
            .map(|(data, mimes)| (data.to_vec(), mimes.iter().map(|s| s.to_string()).collect()))
            .collect();
        let _ = self.request_sender.send(Command::StoreMulti { formats });
    }

    /// Get the list of MIME types available in the clipboard.
    ///
    /// Returns an empty list if the clipboard is empty or inaccessible.
    pub fn available_mime_types(&self) -> Result<Vec<String>> {
        let _ = self.request_sender.send(Command::GetMimeTypes);

        match self.request_receiver.recv() {
            Ok(Ok(Reply::MimeTypes(types))) => Ok(types),
            Ok(Ok(_)) => Ok(Vec::new()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ClipboardError::WorkerDead),
        }
    }

    // ========================================================================
    // Primary Selection - Generic API
    // ========================================================================

    /// Load data from primary selection with preferred MIME types.
    ///
    /// The first available MIME type from `mime_types` will be used.
    pub fn load_primary(&self, mime_types: &[&str]) -> Result<ClipboardData> {
        let mimes: Vec<String> = mime_types.iter().map(|s| s.to_string()).collect();
        let _ = self.request_sender.send(Command::LoadPrimary { mime_types: mimes });

        match self.request_receiver.recv() {
            Ok(Ok(Reply::Data(data))) => Ok(data),
            Ok(Ok(_)) => Err(ClipboardError::Empty),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ClipboardError::WorkerDead),
        }
    }

    /// Store data to primary selection with specified MIME types.
    pub fn store_primary(&self, data: &[u8], mime_types: &[&str]) {
        let request = Command::StorePrimary {
            data: data.to_vec(),
            mime_types: mime_types.iter().map(|s| s.to_string()).collect(),
        };
        let _ = self.request_sender.send(request);
    }

    /// Store multiple formats to primary selection with different data per format.
    ///
    /// See [`store_multi`](Self::store_multi) for details.
    pub fn store_primary_multi(&self, formats: &[(&[u8], &[&str])]) {
        let formats: Vec<(Vec<u8>, Vec<String>)> = formats
            .iter()
            .map(|(data, mimes)| (data.to_vec(), mimes.iter().map(|s| s.to_string()).collect()))
            .collect();
        let _ = self.request_sender.send(Command::StorePrimaryMulti { formats });
    }

    /// Get the list of MIME types available in the primary selection.
    pub fn available_mime_types_primary(&self) -> Result<Vec<String>> {
        let _ = self.request_sender.send(Command::GetPrimaryMimeTypes);

        match self.request_receiver.recv() {
            Ok(Ok(Reply::MimeTypes(types))) => Ok(types),
            Ok(Ok(_)) => Ok(Vec::new()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ClipboardError::WorkerDead),
        }
    }

    // ========================================================================
    // Convenience methods for text
    // ========================================================================

    /// Load text from clipboard.
    ///
    /// This is a convenience method that loads data using common text MIME types
    /// and converts the result to a UTF-8 string.
    pub fn load_text(&self) -> Result<String> {
        let data = self.load(&mime::TEXT_MIME_TYPES)?;
        data.as_text().map(|s| s.to_string()).ok_or(ClipboardError::InvalidUtf8)
    }

    /// Store text to clipboard.
    ///
    /// This is a convenience method that stores text using common text MIME types.
    pub fn store_text(&self, text: impl AsRef<str>) {
        self.store(text.as_ref().as_bytes(), &mime::TEXT_MIME_TYPES);
    }

    /// Load text from primary selection.
    pub fn load_text_primary(&self) -> Result<String> {
        let data = self.load_primary(&mime::TEXT_MIME_TYPES)?;
        data.as_text().map(|s| s.to_string()).ok_or(ClipboardError::InvalidUtf8)
    }

    /// Store text to primary selection.
    pub fn store_text_primary(&self, text: impl AsRef<str>) {
        self.store_primary(text.as_ref().as_bytes(), &mime::TEXT_MIME_TYPES);
    }

    // ========================================================================
    // DnD (Drag and Drop) API - only available with the "dnd" feature
    // ========================================================================

    /// Initialize DnD with an event sender.
    ///
    /// Call this once to receive DnD events through the provided sender.
    /// Events will be sent as [`dnd::DndEvent`] through the channel.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use sctk::reexports::calloop::channel;
    /// # let clipboard: smithay_clipboard::Clipboard = todo!();
    ///
    /// let (sender, _receiver) = channel::channel();
    /// clipboard.init_dnd(Box::new(sender));
    /// ```
    #[cfg(feature = "dnd")]
    pub fn init_dnd(
        &self,
        sender: Box<dyn dnd::Sender<sctk::reexports::client::protocol::wl_surface::WlSurface> + Send>,
    ) {
        let _ = self.request_sender.send(Command::Dnd(DndCommand::InitDnd(sender)));
    }

    /// Register a surface for receiving DnD offers.
    ///
    /// Rectangles define the drop target areas within the surface. They should
    /// be provided in order of decreasing priority. This method can be called
    /// multiple times for a single surface if the rectangles change.
    ///
    /// Call with an empty rectangles list to unregister the surface.
    #[cfg(feature = "dnd")]
    pub fn register_dnd_destination(
        &self,
        surface: sctk::reexports::client::protocol::wl_surface::WlSurface,
        rectangles: Vec<dnd::DndDestinationRectangle>,
    ) {
        let _ = self.request_sender.send(Command::Dnd(DndCommand::RegisterDestination {
            surface,
            rectangles,
        }));
    }

    /// Start a DnD operation on the given surface with some data.
    ///
    /// # Arguments
    ///
    /// * `source` - The surface where the drag originates
    /// * `data` - The data to be dragged
    /// * `actions` - Allowed DnD actions (Copy, Move, Ask, etc.)
    /// * `icon` - Optional icon surface to display during drag
    #[cfg(feature = "dnd")]
    pub fn start_dnd(
        &self,
        source: sctk::reexports::client::protocol::wl_surface::WlSurface,
        data: dnd::DndData,
        actions: sctk::reexports::client::protocol::wl_data_device_manager::DndAction,
        icon: Option<sctk::reexports::client::protocol::wl_surface::WlSurface>,
    ) {
        let _ = self.request_sender.send(Command::Dnd(DndCommand::StartDnd {
            source,
            data,
            actions,
            icon,
        }));
    }

    /// End the current DnD operation.
    ///
    /// Call this to cancel an ongoing drag operation.
    #[cfg(feature = "dnd")]
    pub fn end_dnd(&self) {
        let _ = self.request_sender.send(Command::Dnd(DndCommand::EndDnd));
    }

    /// Set the final action after the user made a choice.
    ///
    /// This should be called when the selected action is `Ask` and the user
    /// has been presented with a choice of actions.
    #[cfg(feature = "dnd")]
    pub fn set_dnd_action(
        &self,
        action: sctk::reexports::client::protocol::wl_data_device_manager::DndAction,
    ) {
        let _ = self.request_sender.send(Command::Dnd(DndCommand::SetAction(action)));
    }

    /// Peek at the contents of a DnD offer.
    ///
    /// This allows reading the drag data before the drop is performed.
    /// Returns the data for the specified MIME type.
    #[cfg(feature = "dnd")]
    pub fn peek_dnd_offer(&self, mime_type: &str) -> Result<ClipboardData> {
        let _ = self.request_sender.send(Command::Dnd(DndCommand::Peek(mime_type.to_string())));

        match self.request_receiver.recv() {
            Ok(Ok(Reply::Data(data))) => Ok(data),
            Ok(Ok(_)) => Err(ClipboardError::Empty),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ClipboardError::WorkerDead),
        }
    }

    /// Finish the DnD operation (accept the dropped data).
    ///
    /// Call this after receiving `OfferEvent::Drop` to complete the operation.
    #[cfg(feature = "dnd")]
    pub fn finish_dnd(&self) {
        let _ = self.request_sender.send(Command::Dnd(DndCommand::Finish));
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // Shutdown smithay-clipboard.
        let _ = self.request_sender.send(Command::Exit);
        if let Some(clipboard_thread) = self.clipboard_thread.take() {
            let _ = clipboard_thread.join();
        }
    }
}

