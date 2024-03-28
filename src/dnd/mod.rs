use std::ffi::c_void;
use std::fmt::Debug;
use std::sync::mpsc::SendError;

use sctk::reexports::calloop;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy};
use wayland_backend::client::{InvalidId, ObjectId};

use crate::mime::{AllowedMimeTypes, AsMimeTypes, MimeType};
use crate::Clipboard;

pub mod state;

#[derive(Clone)]
pub struct DndSurface<T> {
    pub(crate) surface: WlSurface,
    pub s: T,
}

impl<T> Debug for DndSurface<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface").field("surface", &self.surface).finish()
    }
}

impl<T: RawSurface> DndSurface<T> {
    fn new(mut s: T, conn: &Connection) -> Result<Self, InvalidId> {
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

pub trait RawSurface {
    /// # Safety
    ///
    /// returned pointer must be a valid `*mut wl_surface` pointer, and it must
    /// remain valid for as long as `RawSurface` object is alive.
    unsafe fn get_ptr(&mut self) -> *mut c_void;
}

pub trait Sender<T> {
    /// Send an event in the channel
    fn send(&self, t: DndEvent<T>) -> Result<(), SendError<DndEvent<T>>>;
}

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
    Mime(Option<MimeType>),
    /// DnD Dropped. The operation is still ongoing until receiving a
    /// [`SourceEvent::Finished`] event.
    Dropped,
}

#[derive(Debug)]
pub enum OfferEvent<T> {
    Enter {
        x: f64,
        y: f64,
        mime_types: Vec<MimeType>,
        surface: T,
    },
    Motion {
        x: f64,
        y: f64,
    },
    /// The offer is no longer on a DnD destination.
    LeaveDestination,
    /// The offer has left the surface.
    Leave,
    /// An offer was dropped
    Drop,
    /// If the selected action is ASK, the user must be presented with a choice.
    /// [`Clipboard::set_action`] should then be called before data can be
    /// requested and th DnD operation can be finished.
    SelectedAction(DndAction),
    Data {
        data: Vec<u8>,
        mime_type: MimeType,
    },
}

/// A rectangle with a logical location and size relative to a [`DndSurface`]
#[derive(Debug, Default, Clone)]
pub struct Rectangle {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rectangle {
    fn contains(&self, x: f64, y: f64) -> bool {
        self.x <= x && self.x + self.width >= x && self.y <= y && self.y + self.height >= y
    }
}

#[derive(Debug, Clone)]
pub struct DndDestinationRectangle {
    /// A unique ID
    pub id: u128,
    /// The rectangle representing this destination.
    pub rectangle: Rectangle,
    /// Accepted mime types in this rectangle
    pub mime_types: Vec<MimeType>,
    /// Accepted actions in this rectangle
    pub actions: DndAction,
    /// Prefered action in this rectangle
    pub preferred: DndAction,
}

pub enum DndRequest<T> {
    /// Init DnD
    InitDnd(Box<dyn crate::dnd::Sender<T> + Send>),
    /// Register a surface for receiving Dnd events.
    Surface(DndSurface<T>, Vec<DndDestinationRectangle>),
    /// Start a Dnd operation with the given source surface and data.
    StartDnd {
        internal: bool,
        source: DndSurface<T>,
        icon: Option<Icon<DndSurface<T>>>,
        content: Box<dyn AsMimeTypes + Send>,
        actions: DndAction,
    },
    /// Peek the data of an active DnD offer
    Peek(MimeType),
    /// Set the DnD action chosen by the user.
    SetAction(DndAction),
    /// End an active DnD Source
    DndEnd,
}

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

pub enum Icon<S> {
    Surface(S),
    /// Argb8888 or Xrgb8888 encoded image data pre-multiplied by alpha.
    Buf {
        width: u32,
        height: u32,
        data: Vec<u8>,
        transparent: bool,
    },
}

impl<T: RawSurface> Clipboard<T> {
    /// Set up DnD operations for the Clipboard
    pub fn init_dnd(
        &self,
        tx: Box<dyn Sender<T> + Send>,
    ) -> Result<(), SendError<crate::worker::Command<T>>> {
        self.request_sender.send(crate::worker::Command::DndRequest(DndRequest::InitDnd(tx)))
    }

    /// Start a DnD operation on the given surface with some data
    pub fn start_dnd<D: AsMimeTypes + Send + 'static>(
        &self,
        internal: bool,
        source_surface: T,
        icon_surface: Option<Icon<T>>,
        content: D,
        actions: DndAction,
    ) {
        let source = DndSurface::new(source_surface, &self.connection).unwrap();
        let icon = icon_surface.map(|i| match i {
            Icon::Surface(s) => Icon::Surface(DndSurface::new(s, &self.connection).unwrap()),
            Icon::Buf { width, height, data, transparent } => {
                Icon::Buf { width, height, data, transparent }
            },
        });
        _ = self.request_sender.send(crate::worker::Command::DndRequest(DndRequest::StartDnd {
            internal,
            source,
            icon,
            content: Box::new(content),
            actions,
        }));
    }

    /// End the current DnD operation, if there is one
    pub fn end_dnd(&self) {
        _ = self.request_sender.send(crate::worker::Command::DndRequest(DndRequest::DndEnd));
    }

    /// Register a surface for receiving DnD offers
    /// Rectangles should be provided in order of decreasing priority.
    /// This method c~an be called multiple time for a single surface if the
    /// rectangles change.
    pub fn register_dnd_destination(&self, surface: T, rectangles: Vec<DndDestinationRectangle>) {
        let s = DndSurface::new(surface, &self.connection).unwrap();

        _ = self
            .request_sender
            .send(crate::worker::Command::DndRequest(DndRequest::Surface(s, rectangles)));
    }

    /// Set the final action after presenting the user with a choice
    pub fn set_action(&self, action: DndAction) {
        _ = self
            .request_sender
            .send(crate::worker::Command::DndRequest(DndRequest::SetAction(action)));
    }

    /// Peek at the contents of a DnD offer
    pub fn peek_offer<D: AllowedMimeTypes + 'static>(
        &self,
        mime_type: Option<MimeType>,
    ) -> std::io::Result<D> {
        let Some(mime_type) = mime_type.or_else(|| D::allowed().first().cloned()) else {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "No mime type provided."));
        };

        self.request_sender
            .send(crate::worker::Command::DndRequest(DndRequest::Peek(mime_type)))
            .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::Other, "Failed to send Peek request.")
        })?;

        self.request_receiver
            .recv()
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::Other, "Failed to receive data request.")
            })
            .and_then(|ret| {
                D::try_from(ret?).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Failed to convert data to requested type.",
                    )
                })
            })
    }
}
