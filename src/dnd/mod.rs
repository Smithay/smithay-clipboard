//! Drag and Drop (DnD) support for Wayland.
//!
//! This module provides functionality for drag and drop operations in Wayland,
//! including both source (dragging) and destination (dropping) handling.
//!
//! # Example
//!
//! ```no_run
//! use smithay_clipboard::dnd::{DndDestinationRectangle, Rectangle, DndEvent, OfferEvent};
//! use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
//!
//! // Register a surface for receiving DnD offers
//! // clipboard.register_dnd_destination(surface, vec![
//! //     DndDestinationRectangle {
//! //         id: 1,
//! //         rectangle: Rectangle { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
//! //         mime_types: vec!["text/plain".into()],
//! //         actions: DndAction::Copy,
//! //         preferred: DndAction::Copy,
//! //     },
//! // ]);
//! ```

use std::ffi::c_void;
use std::fmt::Debug;
use std::sync::mpsc::SendError;

use sctk::reexports::calloop;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy};
use wayland_backend::client::{InvalidId, ObjectId};

pub mod state;

/// A surface wrapper for DnD operations.
#[derive(Clone)]
pub struct DndSurface<T> {
    pub(crate) surface: WlSurface,
    /// The original surface handle.
    pub s: T,
}

impl<T> Debug for DndSurface<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DndSurface").field("surface", &self.surface).finish()
    }
}

impl<T: RawSurface> DndSurface<T> {
    /// Create a new DnD surface from a raw surface.
    pub fn new(mut s: T, conn: &Connection) -> Result<Self, InvalidId> {
        let ptr = unsafe { s.get_ptr() };
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), ptr.cast())? };
        let surface = WlSurface::from_id(conn, id)?;
        Ok(Self { s, surface })
    }
}

#[cfg(feature = "rwh-6")]
impl<'a> RawSurface for raw_window_handle::WindowHandle<'a> {
    unsafe fn get_ptr(&mut self) -> *mut c_void {
        match self.as_raw() {
            raw_window_handle::RawWindowHandle::Wayland(handle) => handle.surface.as_ptr().cast(),
            _ => panic!("Unsupported window handle type."),
        }
    }
}

impl RawSurface for WlSurface {
    unsafe fn get_ptr(&mut self) -> *mut c_void {
        self.id().as_ptr().cast()
    }
}

/// A trait for types that can provide a raw Wayland surface pointer.
pub trait RawSurface {
    /// Get a raw pointer to the underlying `wl_surface`.
    ///
    /// # Safety
    ///
    /// The returned pointer must be a valid `*mut wl_surface` pointer, and it must
    /// remain valid for as long as the `RawSurface` object is alive.
    unsafe fn get_ptr(&mut self) -> *mut c_void;
}

/// A trait for sending DnD events.
pub trait Sender<T> {
    /// Send an event in the channel.
    fn send(&self, t: DndEvent<T>) -> Result<(), SendError<DndEvent<T>>>;
}

/// Events from a DnD source operation.
#[derive(Debug)]
pub enum SourceEvent {
    /// DnD operation ended.
    Finished,
    /// DnD Cancelled.
    Cancelled,
    /// DnD action chosen by the compositor.
    Action(DndAction),
    /// Mime accepted by destination.
    /// If [`None`], no mime types are accepted.
    Mime(Option<String>),
    /// DnD Dropped. The operation is still ongoing until receiving a
    /// [`SourceEvent::Finished`] event.
    Dropped,
}

/// Events from a DnD offer (destination) operation.
#[derive(Debug)]
pub enum OfferEvent<T> {
    /// A drag entered the surface.
    Enter {
        /// X coordinate relative to the surface.
        x: f64,
        /// Y coordinate relative to the surface.
        y: f64,
        /// MIME types offered by the source.
        mime_types: Vec<String>,
        /// The surface that received the enter event.
        surface: T,
    },
    /// The drag moved over the surface.
    Motion {
        /// X coordinate relative to the surface.
        x: f64,
        /// Y coordinate relative to the surface.
        y: f64,
    },
    /// The offer is no longer on a DnD destination rectangle.
    LeaveDestination,
    /// The offer has left the surface.
    Leave,
    /// An offer was dropped.
    Drop,
    /// If the selected action is ASK, the user must be presented with a choice.
    /// [`Clipboard::set_action`] should then be called before data can be
    /// requested and the DnD operation can be finished.
    SelectedAction(DndAction),
    /// Data received from the DnD source.
    Data {
        /// The data bytes.
        data: Vec<u8>,
        /// The MIME type of the data.
        mime_type: String,
    },
}

/// A rectangle with a logical location and size relative to a [`DndSurface`].
#[derive(Debug, Default, Clone)]
pub struct Rectangle {
    /// X coordinate of the top-left corner.
    pub x: f64,
    /// Y coordinate of the top-left corner.
    pub y: f64,
    /// Width of the rectangle.
    pub width: f64,
    /// Height of the rectangle.
    pub height: f64,
}

impl Rectangle {
    /// Check if a point is contained within the rectangle.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.x <= x && self.x + self.width >= x && self.y <= y && self.y + self.height >= y
    }
}

/// A destination rectangle for DnD operations.
#[derive(Debug, Clone)]
pub struct DndDestinationRectangle {
    /// A unique ID for this destination.
    pub id: u128,
    /// The rectangle representing this destination.
    pub rectangle: Rectangle,
    /// Accepted mime types in this rectangle.
    pub mime_types: Vec<String>,
    /// Accepted actions in this rectangle.
    pub actions: DndAction,
    /// Preferred action in this rectangle.
    pub preferred: DndAction,
}

/// Requests for DnD operations.
pub enum DndRequest<T> {
    /// Initialize DnD with an event sender.
    InitDnd(Box<dyn Sender<T> + Send>),
    /// Register a surface for receiving Dnd events.
    Surface(DndSurface<T>, Vec<DndDestinationRectangle>),
    /// Start a Dnd operation with the given source surface and data.
    StartDnd {
        /// Whether this is an internal DnD operation within the application.
        internal: bool,
        /// The source surface for the drag.
        source: DndSurface<T>,
        /// Optional icon surface for the drag.
        icon: Option<Icon<DndSurface<T>>>,
        /// The data to be dragged.
        content: DndData,
        /// Allowed DnD actions.
        actions: DndAction,
    },
    /// Peek the data of an active DnD offer.
    Peek(String),
    /// Set the DnD action chosen by the user.
    SetAction(DndAction),
    /// End an active DnD Source.
    DndEnd,
}

impl<T> Debug for DndRequest<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InitDnd(_) => f.debug_tuple("InitDnd").finish(),
            Self::Surface(surface, rects) => {
                f.debug_tuple("Surface").field(surface).field(rects).finish()
            }
            Self::StartDnd { internal, source, icon, content, actions } => f
                .debug_struct("StartDnd")
                .field("internal", internal)
                .field("source", source)
                .field("icon", icon)
                .field("content", content)
                .field("actions", actions)
                .finish(),
            Self::Peek(mime) => f.debug_tuple("Peek").field(mime).finish(),
            Self::SetAction(action) => f.debug_tuple("SetAction").field(action).finish(),
            Self::DndEnd => write!(f, "DndEnd"),
        }
    }
}

/// Data for DnD operations.
#[derive(Debug, Clone)]
pub struct DndData {
    /// The MIME types this data is available in.
    pub mime_types: Vec<String>,
    /// The actual data.
    pub data: Vec<u8>,
}

impl DndData {
    /// Create new DnD data.
    pub fn new(data: impl Into<Vec<u8>>, mime_types: Vec<String>) -> Self {
        Self { data: data.into(), mime_types }
    }

    /// Create DnD data from text.
    pub fn from_text(text: impl AsRef<str>) -> Self {
        Self {
            data: text.as_ref().as_bytes().to_vec(),
            mime_types: vec![
                "text/plain;charset=utf-8".into(),
                "UTF8_STRING".into(),
                "text/plain".into(),
            ],
        }
    }
}

/// A DnD event.
#[derive(Debug)]
pub enum DndEvent<T> {
    /// Dnd Offer event with the corresponding destination rectangle ID.
    Offer(Option<u128>, OfferEvent<T>),
    /// Dnd Source event.
    Source(SourceEvent),
}

impl<T> Sender<T> for calloop::channel::Sender<DndEvent<T>> {
    fn send(&self, t: DndEvent<T>) -> Result<(), SendError<DndEvent<T>>> {
        self.send(t)
    }
}

impl<T> Sender<T> for calloop::channel::SyncSender<DndEvent<T>> {
    fn send(&self, t: DndEvent<T>) -> Result<(), SendError<DndEvent<T>>> {
        self.send(t)
    }
}

/// An icon for a DnD drag operation.
#[derive(Debug)]
pub enum Icon<S> {
    /// Use a surface as the icon.
    Surface(S),
    /// Use pixel data as the icon (Argb8888 or Xrgb8888 encoded, pre-multiplied by alpha).
    Buffer {
        /// Width of the icon in pixels.
        width: u32,
        /// Height of the icon in pixels.
        height: u32,
        /// The pixel data.
        data: Vec<u8>,
        /// Whether the icon has transparency.
        transparent: bool,
    },
}
