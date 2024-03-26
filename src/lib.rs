//! Smithay Clipboard
//!
//! Provides access to the Wayland clipboard for gui applications. The user
//! should have surface around.

#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
use std::borrow::Cow;
use std::ffi::c_void;
use std::io::Result;
use std::sync::mpsc::{self, Receiver};

use sctk::reexports::calloop::channel::{self, Sender};
use sctk::reexports::client::backend::Backend;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::Connection;

#[cfg(feature = "dnd")]
pub mod dnd;
pub mod mime;
mod state;
pub mod text;
mod worker;

use mime::{AllowedMimeTypes, AsMimeTypes, MimeType};
use state::Target;
use text::Text;

pub type SimpleClipboard = Clipboard<WlSurface>;

/// Access to a Wayland clipboard.
pub struct Clipboard<T> {
    request_sender: Sender<worker::Command<T>>,
    request_receiver: Receiver<Result<(Vec<u8>, MimeType)>>,
    clipboard_thread: Option<std::thread::JoinHandle<()>>,
    connection: Connection,
}

impl<T: 'static + Send + Clone> Clipboard<T> {
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
        let clipboard_thread =
            worker::spawn(name, connection.clone(), rx_chan, clipboard_reply_sender);

        Self { request_receiver, request_sender, clipboard_thread, connection }
    }

    /// Load custom clipboard data.
    ///
    /// Load the requested type from a clipboard on the last observed seat.
    pub fn load<D: AllowedMimeTypes + 'static>(&self) -> Result<D> {
        self.load_inner(Target::Clipboard, D::allowed())
    }

    /// Load clipboard data.
    ///
    /// Loads content from a clipboard on a last observed seat.
    pub fn load_text(&self) -> Result<String> {
        self.load::<Text>().map(|t| t.0)
    }

    /// Load custom primary clipboard data.
    ///
    /// Load the requested type from a primary clipboard on the last observed
    /// seat.
    pub fn load_primary<D: AllowedMimeTypes + 'static>(&self) -> Result<D> {
        self.load_inner(Target::Primary, D::allowed())
    }

    /// Load primary clipboard data.
    ///
    /// Loads content from a  primary clipboard on a last observed seat.
    pub fn load_primary_text(&self) -> Result<String> {
        self.load_primary::<Text>().map(|t| t.0)
    }

    /// Load clipboard data for sepecific mime types.
    ///
    /// Loads content from a  primary clipboard on a last observed seat.
    pub fn load_mime<D: TryFrom<(Vec<u8>, MimeType)>>(
        &self,
        allowed: impl Into<Cow<'static, [MimeType]>>,
    ) -> Result<D> {
        self.load_inner(Target::Clipboard, allowed).and_then(|d| {
            D::try_from(d).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to load data of the requested type.",
                )
            })
        })
    }

    /// Load primary clipboard data for specific mime types.
    ///
    /// Loads content from a  primary clipboard on a last observed seat.
    pub fn load_primary_mime<D: TryFrom<(Vec<u8>, MimeType)>>(
        &self,
        allowed: impl Into<Cow<'static, [MimeType]>>,
    ) -> Result<D> {
        self.load_inner(Target::Primary, allowed).and_then(|d| {
            D::try_from(d).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to load data of the requested type.",
                )
            })
        })
    }

    /// Store custom data to a clipboard.
    ///
    /// Stores data of the provided type to a clipboard on a last observed seat.
    pub fn store<D: AsMimeTypes + Send + 'static>(&self, data: D) {
        self.store_inner(data, Target::Clipboard);
    }

    /// Store to a clipboard.
    ///
    /// Stores to a clipboard on a last observed seat.
    pub fn store_text<D: Into<String>>(&self, text: D) {
        self.store(Text(text.into()));
    }

    /// Store custom data to a primary clipboard.
    ///
    /// Stores data of the provided type to a primary clipboard on a last
    /// observed seat.
    pub fn store_primary<D: AsMimeTypes + Send + 'static>(&self, data: D) {
        self.store_inner(data, Target::Primary);
    }

    /// Store to a primary clipboard.
    ///
    /// Stores to a primary clipboard on a last observed seat.
    pub fn store_primary_text<D: Into<String>>(&self, text: D) {
        self.store_primary(Text(text.into()));
    }

    fn load_inner<D: TryFrom<(Vec<u8>, MimeType)> + 'static>(
        &self,
        target: Target,
        allowed: impl Into<Cow<'static, [MimeType]>>,
    ) -> Result<D> {
        let _ = self.request_sender.send(worker::Command::Load(allowed.into(), target));

        match self.request_receiver.recv() {
            Ok(res) => res.and_then(|(data, mime)| {
                D::try_from((data, mime)).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Failed to load data of the requested type.",
                    )
                })
            }),
            // The clipboard thread is dead, however we shouldn't crash downstream,
            // so propogating an error.
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::Other, "clipboard is dead.")),
        }
    }

    fn store_inner<D: AsMimeTypes + Send + 'static>(&self, data: D, target: Target) {
        let request = worker::Command::Store(Box::new(data), target);
        let _ = self.request_sender.send(request);
    }
}

impl<T> Drop for Clipboard<T> {
    fn drop(&mut self) {
        // Shutdown smithay-clipboard.
        let _ = self.request_sender.send(worker::Command::Exit);
        if let Some(clipboard_thread) = self.clipboard_thread.take() {
            let _ = clipboard_thread.join();
        }
    }
}
