//! Smithay Clipboard
//!
//! Provides access to the wayland clipboard with only requirement being a WlDisplay
//! object
//!
//! ```no_run
//! let (display, _) =
//! Display::connect_to_env().expect("Failed to connect to the wayland server.");
//! let mut clipboard = smithay_clipboard::WaylandClipboard::new(&display);
//! clipboard.store(None, "Test data");
//! println!("{}", clipboard.load(None));
//! ```

#![warn(missing_docs)]

mod threaded;
pub use threaded::ThreadedClipboard;
pub use threaded::ThreadedClipboard as WaylandClipboard;
