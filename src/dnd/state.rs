//! DnD state handling for the clipboard worker.

use std::collections::HashMap;

use sctk::data_device_manager::data_offer::DragOffer;
use sctk::reexports::client::protocol::wl_buffer::WlBuffer;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use sctk::shm::multi::MultiPool;
use sctk::shm::Shm;
use wayland_backend::client::ObjectId;

use super::{
    DndData, DndDestinationRectangle, DndEvent, OfferEvent, Sender, SourceEvent,
};

/// DnD state for a single seat.
#[derive(Default)]
pub struct SeatDndState {
    /// Current drag offer on this seat.
    pub drag_offer: Option<DragOfferState>,
    /// Current drag source on this seat.
    pub drag_source: Option<DragSourceState>,
}

/// State for an active drag offer (destination).
pub struct DragOfferState {
    /// The offer from the data device.
    pub offer: DragOffer,
    /// MIME types offered by the source.
    pub mime_types: Vec<String>,
    /// Current X position.
    pub x: f64,
    /// Current Y position.
    pub y: f64,
    /// The surface that received the enter.
    pub surface: WlSurface,
    /// Whether the offer has left the surface.
    pub left: bool,
}

/// State for an active drag source.
pub struct DragSourceState {
    /// The data source.
    pub source: WlDataSource,
    /// The data to offer.
    pub data: DndData,
    /// Whether this is internal DnD.
    pub internal: bool,
}

/// State for registered DnD destination surfaces.
#[derive(Default)]
pub struct DndDestinationState<T> {
    /// Maps surface ID to (surface handle, rectangles).
    pub surfaces: HashMap<ObjectId, (T, Vec<DndDestinationRectangle>)>,
    /// Currently matched rectangle ID.
    pub current_rectangle: Option<u128>,
}

impl<T> DndDestinationState<T> {
    /// Find the rectangle that contains the given point on a surface.
    pub fn find_rectangle(&self, surface: &WlSurface, x: f64, y: f64) -> Option<&DndDestinationRectangle> {
        let (_, rectangles) = self.surfaces.get(&surface.id())?;
        rectangles.iter().find(|r| r.rectangle.contains(x, y))
    }

    /// Register a surface for DnD destination.
    pub fn register(&mut self, surface: &WlSurface, handle: T, rectangles: Vec<DndDestinationRectangle>) {
        self.surfaces.insert(surface.id(), (handle, rectangles));
    }

    /// Unregister a surface.
    pub fn unregister(&mut self, surface: &WlSurface) {
        self.surfaces.remove(&surface.id());
    }
}

/// DnD icon state.
pub struct DndIconState {
    /// The icon surface.
    pub surface: WlSurface,
    /// The buffer attached to the surface.
    pub buffer: WlBuffer,
    /// Pool used for the icon.
    pub pool: MultiPool<()>,
}

impl DndIconState {
    /// Create an icon from pixel data.
    pub fn from_data<S: Clone>(
        _conn: &Connection,
        _qh: &QueueHandle<S>,
        _shm: &Shm,
        _width: u32,
        _height: u32,
        _data: &[u8],
        _transparent: bool,
    ) -> Option<Self>
    where
        S: 'static,
    {
        // We need a compositor state to create surfaces, but we don't have access
        // to it here. The icon creation should happen at a higher level.
        // This is a placeholder for now.
        None
    }
}

/// Handle DnD data device events and dispatch to the sender.
pub fn handle_dnd_enter<T: Clone>(
    sender: &Option<Box<dyn Sender<T> + Send>>,
    destinations: &mut DndDestinationState<T>,
    surface: &WlSurface,
    x: f64,
    y: f64,
    mime_types: Vec<String>,
) {
    let Some(sender) = sender.as_ref() else {
        return;
    };

    // Find the surface handle
    let Some((handle, _)) = destinations.surfaces.get(&surface.id()) else {
        return;
    };

    // Find the matching rectangle
    let rect_id = destinations.find_rectangle(surface, x, y).map(|r| r.id);
    destinations.current_rectangle = rect_id;

    let event = OfferEvent::Enter { x, y, mime_types, surface: handle.clone() };
    let _ = sender.send(DndEvent::Offer(rect_id, event));
}

/// Handle DnD motion events.
pub fn handle_dnd_motion<T: Clone>(
    sender: &Option<Box<dyn Sender<T> + Send>>,
    destinations: &mut DndDestinationState<T>,
    surface: &WlSurface,
    x: f64,
    y: f64,
) {
    let Some(sender) = sender.as_ref() else {
        return;
    };

    // Find the new matching rectangle
    let new_rect_id = destinations.find_rectangle(surface, x, y).map(|r| r.id);
    let old_rect_id = destinations.current_rectangle;

    // If we changed rectangles, send leave destination event
    if old_rect_id != new_rect_id {
        if old_rect_id.is_some() {
            let _ = sender.send(DndEvent::Offer(old_rect_id, OfferEvent::LeaveDestination));
        }
        destinations.current_rectangle = new_rect_id;
    }

    let event = OfferEvent::Motion { x, y };
    let _ = sender.send(DndEvent::Offer(new_rect_id, event));
}

/// Handle DnD leave events.
pub fn handle_dnd_leave<T: Clone>(
    sender: &Option<Box<dyn Sender<T> + Send>>,
    destinations: &mut DndDestinationState<T>,
) {
    let Some(sender) = sender.as_ref() else {
        return;
    };

    let rect_id = destinations.current_rectangle.take();
    let _ = sender.send(DndEvent::Offer(rect_id, OfferEvent::Leave));
}

/// Handle DnD drop events.
pub fn handle_dnd_drop<T: Clone>(
    sender: &Option<Box<dyn Sender<T> + Send>>,
    destinations: &DndDestinationState<T>,
) {
    let Some(sender) = sender.as_ref() else {
        return;
    };

    let rect_id = destinations.current_rectangle;
    let _ = sender.send(DndEvent::Offer(rect_id, OfferEvent::Drop));
}

/// Handle DnD selected action events.
pub fn handle_dnd_selected_action<T: Clone>(
    sender: &Option<Box<dyn Sender<T> + Send>>,
    destinations: &DndDestinationState<T>,
    action: DndAction,
) {
    let Some(sender) = sender.as_ref() else {
        return;
    };

    let rect_id = destinations.current_rectangle;
    let _ = sender.send(DndEvent::Offer(rect_id, OfferEvent::SelectedAction(action)));
}

/// Handle source cancelled events.
pub fn handle_source_cancelled<T>(sender: &Option<Box<dyn Sender<T> + Send>>) {
    if let Some(sender) = sender.as_ref() {
        let _ = sender.send(DndEvent::Source(SourceEvent::Cancelled));
    }
}

/// Handle source finished events.
pub fn handle_source_finished<T>(sender: &Option<Box<dyn Sender<T> + Send>>) {
    if let Some(sender) = sender.as_ref() {
        let _ = sender.send(DndEvent::Source(SourceEvent::Finished));
    }
}

/// Handle source dropped events.
pub fn handle_source_dropped<T>(sender: &Option<Box<dyn Sender<T> + Send>>) {
    if let Some(sender) = sender.as_ref() {
        let _ = sender.send(DndEvent::Source(SourceEvent::Dropped));
    }
}

/// Handle source action events.
pub fn handle_source_action<T>(sender: &Option<Box<dyn Sender<T> + Send>>, action: DndAction) {
    if let Some(sender) = sender.as_ref() {
        let _ = sender.send(DndEvent::Source(SourceEvent::Action(action)));
    }
}

/// Handle source mime accepted events.
pub fn handle_source_mime<T>(sender: &Option<Box<dyn Sender<T> + Send>>, mime: Option<String>) {
    if let Some(sender) = sender.as_ref() {
        let _ = sender.send(DndEvent::Source(SourceEvent::Mime(mime)));
    }
}
