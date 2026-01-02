use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::mem;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::mpsc::Sender;

use sctk::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferError, DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler, DragSource};
use sctk::data_device_manager::{DataDeviceManagerState, WritePipe};
use sctk::primary_selection::PrimarySelectionManagerState;
use sctk::primary_selection::device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler};
use sctk::primary_selection::selection::{PrimarySelectionSource, PrimarySelectionSourceHandler};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::pointer::{PointerData, PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::{
    delegate_data_device, delegate_pointer, delegate_primary_selection, delegate_registry,
    delegate_seat, registry_handlers,
};

use sctk::reexports::calloop::{LoopHandle, PostAction};
use sctk::reexports::client::globals::GlobalList;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_keyboard::WlKeyboard;
use sctk::reexports::client::protocol::wl_pointer::WlPointer;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};
use wayland_backend::client::ObjectId;

use crate::data::ClipboardData;
use crate::error::{ClipboardError, Result};
use crate::mime::{find_preferred_mime, is_text_mime, normalize_to_lf};
use crate::worker::Reply;

#[cfg(feature = "dnd")]
use crate::dnd::{
    DndData, DndDestinationRectangle, Sender as DndSender,
    state::{
        DndDestinationState, DragOfferState,
        handle_dnd_enter, handle_dnd_leave, handle_dnd_motion, handle_dnd_drop,
        handle_dnd_selected_action, handle_source_action, handle_source_cancelled,
        handle_source_dropped, handle_source_finished, handle_source_mime,
    },
};

pub struct State {
    pub primary_selection_manager_state: Option<PrimarySelectionManagerState>,
    pub data_device_manager_state: Option<DataDeviceManagerState>,
    pub reply_tx: Sender<Result<Reply>>,
    pub exit: bool,

    registry_state: RegistryState,
    seat_state: SeatState,

    seats: HashMap<ObjectId, ClipboardSeatState>,
    /// The latest seat which got an event.
    latest_seat: Option<ObjectId>,

    loop_handle: LoopHandle<'static, Self>,
    queue_handle: QueueHandle<Self>,

    primary_sources: Vec<PrimarySelectionSource>,
    /// Maps MIME type -> data for primary selection (multi-format support).
    primary_selection_data: Rc<HashMap<String, Vec<u8>>>,

    data_sources: Vec<CopyPasteSource>,
    /// Maps MIME type -> data for clipboard (multi-format support).
    data_selection_data: Rc<HashMap<String, Vec<u8>>>,

    // DnD-specific state (only available with the "dnd" feature)
    #[cfg(feature = "dnd")]
    pub dnd_destinations: DndDestinationState<WlSurface>,
    #[cfg(feature = "dnd")]
    pub dnd_sender: Option<Box<dyn DndSender<WlSurface> + Send>>,
    #[cfg(feature = "dnd")]
    pub dnd_source: Option<DragSource>,
    #[cfg(feature = "dnd")]
    pub dnd_source_data: Option<DndData>,
    #[cfg(feature = "dnd")]
    pub current_drag_offer: Option<DragOfferState>,
}

impl State {
    #[must_use]
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<Self>,
        loop_handle: LoopHandle<'static, Self>,
        reply_tx: Sender<Result<Reply>>,
    ) -> Option<Self> {
        // NOTE: while it's mutable, it's not part of the hash compute.
        #[allow(clippy::mutable_key_type)]
        let mut seats = HashMap::new();

        let data_device_manager_state = DataDeviceManagerState::bind(globals, queue_handle).ok();
        let primary_selection_manager_state =
            PrimarySelectionManagerState::bind(globals, queue_handle).ok();

        // When both globals are not available nothing could be done.
        if data_device_manager_state.is_none() && primary_selection_manager_state.is_none() {
            return None;
        }

        let seat_state = SeatState::new(globals, queue_handle);
        for seat in seat_state.seats() {
            seats.insert(seat.id(), Default::default());
        }

        Some(Self {
            registry_state: RegistryState::new(globals),
            primary_selection_data: Rc::new(HashMap::new()),
            data_selection_data: Rc::new(HashMap::new()),
            queue_handle: queue_handle.clone(),
            primary_selection_manager_state,
            primary_sources: Vec::new(),
            data_device_manager_state,
            data_sources: Vec::new(),
            latest_seat: None,
            loop_handle,
            exit: false,
            seat_state,
            reply_tx,
            seats,
            #[cfg(feature = "dnd")]
            dnd_destinations: DndDestinationState { surfaces: HashMap::new(), current_rectangle: None },
            #[cfg(feature = "dnd")]
            dnd_sender: None,
            #[cfg(feature = "dnd")]
            dnd_source: None,
            #[cfg(feature = "dnd")]
            dnd_source_data: None,
            #[cfg(feature = "dnd")]
            current_drag_offer: None,
        })
    }

    /// Store selection for the given target with multi-format support.
    ///
    /// Each entry in `formats` is a tuple of (data, mime_types). The same data
    /// will be offered for all MIME types in its associated list.
    ///
    /// Selection source is only created when `Some(())` is returned.
    pub fn store_selection(
        &mut self,
        ty: SelectionTarget,
        formats: Vec<(Vec<u8>, Vec<String>)>,
    ) -> Option<()> {
        let latest = self.latest_seat.as_ref()?;
        let seat = self.seats.get_mut(latest)?;

        if !seat.has_focus {
            return None;
        }

        // Build the MIME -> data mapping
        let mut data_map = HashMap::new();
        let mut all_mimes = Vec::new();
        for (data, mimes) in formats {
            for mime in mimes {
                all_mimes.push(mime.clone());
                data_map.insert(mime, data.clone());
            }
        }

        let data_map = Rc::new(data_map);

        match ty {
            SelectionTarget::Clipboard => {
                let mgr = self.data_device_manager_state.as_ref()?;
                self.data_selection_data = data_map;
                let source = mgr.create_copy_paste_source(
                    &self.queue_handle,
                    all_mimes.iter().map(|s| s.as_str()),
                );
                source.set_selection(seat.data_device.as_ref().unwrap(), seat.latest_serial);
                self.data_sources.push(source);
            },
            SelectionTarget::Primary => {
                let mgr = self.primary_selection_manager_state.as_ref()?;
                self.primary_selection_data = data_map;
                let source = mgr.create_selection_source(
                    &self.queue_handle,
                    all_mimes.iter().map(|s| s.as_str()),
                );
                source.set_selection(seat.primary_device.as_ref().unwrap(), seat.latest_serial);
                self.primary_sources.push(source);
            },
        }

        Some(())
    }

    /// Get available MIME types from the selection.
    pub fn get_mime_types(&mut self, ty: SelectionTarget) -> Result<Vec<String>> {
        let latest = self.latest_seat.as_ref().ok_or(ClipboardError::NoSeat)?;
        let seat = self.seats.get_mut(latest).ok_or(ClipboardError::NoSeat)?;

        if !seat.has_focus {
            return Err(ClipboardError::NoFocus);
        }

        match ty {
            SelectionTarget::Clipboard => {
                let selection = seat
                    .data_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or(ClipboardError::Empty)?;

                Ok(selection.with_mime_types(|mimes| mimes.to_vec()))
            },
            SelectionTarget::Primary => {
                let selection = seat
                    .primary_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or(ClipboardError::Empty)?;

                Ok(selection.with_mime_types(|mimes| mimes.to_vec()))
            },
        }
    }

    /// Load selection for the given target with preferred MIME types.
    pub fn load_selection(
        &mut self,
        ty: SelectionTarget,
        preferred_mimes: &[String],
    ) -> Result<()> {
        let latest = self.latest_seat.as_ref().ok_or(ClipboardError::NoSeat)?;
        let seat = self.seats.get_mut(latest).ok_or(ClipboardError::NoSeat)?;

        if !seat.has_focus {
            return Err(ClipboardError::NoFocus);
        }

        // Convert preferred mimes to &str for matching
        let preferred_refs: Vec<&str> = preferred_mimes.iter().map(|s| s.as_str()).collect();

        let (read_pipe, mime_type) = match ty {
            SelectionTarget::Clipboard => {
                let selection = seat
                    .data_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or(ClipboardError::Empty)?;

                let mime_type = selection
                    .with_mime_types(|offered| find_preferred_mime(offered, &preferred_refs))
                    .ok_or(ClipboardError::NoCompatibleMime)?
                    .to_string();

                let pipe = selection.receive(mime_type.clone()).map_err(|err| match err {
                    DataOfferError::InvalidReceive => {
                        ClipboardError::Io(std::io::Error::other("offer is not ready yet"))
                    },
                    DataOfferError::Io(err) => ClipboardError::Io(err),
                })?;

                (pipe, mime_type)
            },
            SelectionTarget::Primary => {
                let selection = seat
                    .primary_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or(ClipboardError::Empty)?;

                let mime_type = selection
                    .with_mime_types(|offered| find_preferred_mime(offered, &preferred_refs))
                    .ok_or(ClipboardError::NoCompatibleMime)?
                    .to_string();

                let pipe = selection.receive(mime_type.clone())?;

                (pipe, mime_type)
            },
        };

        // Mark FD as non-blocking so we won't block ourselves.
        set_non_blocking(read_pipe.as_raw_fd())?;

        let is_text = is_text_mime(&mime_type);
        let mut reader_buffer = [0; 4096];
        let mut content = Vec::new();
        let _ = self.loop_handle.insert_source(read_pipe, move |_, file, state| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.read(&mut reader_buffer) {
                    Ok(0) => {
                        // For text MIME types, normalize line endings
                        let final_data = if is_text {
                            let text = String::from_utf8_lossy(&content);
                            let normalized = normalize_to_lf(text.into_owned());
                            normalized.into_bytes()
                        } else {
                            mem::take(&mut content)
                        };

                        let data = ClipboardData::new(mime_type.clone(), final_data);
                        let _ = state.reply_tx.send(Ok(Reply::Data(data)));
                        break PostAction::Remove;
                    },
                    Ok(n) => content.extend_from_slice(&reader_buffer[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(err) => {
                        let _ = state.reply_tx.send(Err(ClipboardError::Io(err)));
                        break PostAction::Remove;
                    },
                };
            }
        });

        Ok(())
    }

    fn send_request(&mut self, ty: SelectionTarget, write_pipe: WritePipe, mime: String) {
        // Look up the data for this specific MIME type
        let data_map = match ty {
            SelectionTarget::Clipboard => self.data_selection_data.clone(),
            SelectionTarget::Primary => self.primary_selection_data.clone(),
        };

        // Get the data for this MIME type
        let contents: Rc<[u8]> = match data_map.get(&mime) {
            Some(data) => Rc::from(data.clone().into_boxed_slice()),
            None => return, // MIME type not offered
        };

        // Mark FD as non-blocking so we won't block ourselves.
        if set_non_blocking(write_pipe.as_raw_fd()).is_err() {
            return;
        }

        let mut written = 0;
        let _ = self.loop_handle.insert_source(write_pipe, move |_, file, _| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.write(&contents[written..]) {
                    Ok(n) if written + n == contents.len() => {
                        written += n;
                        break PostAction::Remove;
                    },
                    Ok(n) => written += n,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(_) => break PostAction::Remove,
                }
            }
        });
    }

    // ========================================================================
    // DnD-specific methods (only available with the "dnd" feature)
    // ========================================================================

    /// Initialize DnD with an event sender.
    #[cfg(feature = "dnd")]
    pub fn init_dnd(&mut self, sender: Box<dyn DndSender<WlSurface> + Send>) {
        self.dnd_sender = Some(sender);
    }

    /// Register a surface for receiving DnD offers.
    #[cfg(feature = "dnd")]
    pub fn register_dnd_destination(
        &mut self,
        surface: WlSurface,
        rectangles: Vec<DndDestinationRectangle>,
    ) {
        if rectangles.is_empty() {
            self.dnd_destinations.unregister(&surface);
        } else {
            self.dnd_destinations.register(&surface, surface.clone(), rectangles);
        }
    }

    /// Start a DnD operation.
    #[cfg(feature = "dnd")]
    pub fn start_dnd(
        &mut self,
        source_surface: &WlSurface,
        data: DndData,
        actions: DndAction,
        icon: Option<&WlSurface>,
    ) -> Option<()> {
        let latest = self.latest_seat.as_ref()?;
        let seat = self.seats.get(latest)?;

        let mgr = self.data_device_manager_state.as_ref()?;
        let data_device = seat.data_device.as_ref()?;

        // Create a drag source with the offered MIME types
        let source = mgr.create_drag_and_drop_source(
            &self.queue_handle,
            data.mime_types.iter().map(|s| s.as_str()),
            actions,
        );

        // Start the drag
        source.start_drag(
            data_device,
            source_surface,
            icon,
            seat.latest_serial,
        );

        self.dnd_source = Some(source);
        self.dnd_source_data = Some(data);

        Some(())
    }

    /// End the current DnD operation.
    #[cfg(feature = "dnd")]
    pub fn end_dnd(&mut self) {
        if let Some(source) = self.dnd_source.take() {
            source.inner().destroy();
        }
        self.dnd_source_data = None;
    }

    /// Set the action for the current DnD offer.
    #[cfg(feature = "dnd")]
    pub fn set_dnd_action(&mut self, action: DndAction) {
        if let Some(ref offer_state) = self.current_drag_offer {
            // Only set if we have a valid preferred action
            if action != DndAction::empty() {
                offer_state.offer.set_actions(action, action);
            }
        }
    }

    /// Peek at the data of a DnD offer.
    #[cfg(feature = "dnd")]
    pub fn peek_dnd_offer(&mut self, mime_type: &str) -> Result<()> {
        let offer_state = self.current_drag_offer.as_ref().ok_or(ClipboardError::Empty)?;

        if !offer_state.mime_types.contains(&mime_type.to_string()) {
            return Err(ClipboardError::NoCompatibleMime);
        }

        let read_pipe = offer_state.offer.receive(mime_type.to_string())?;

        set_non_blocking(read_pipe.as_raw_fd())?;

        let mime = mime_type.to_string();
        let is_text = is_text_mime(&mime);
        let mut reader_buffer = [0; 4096];
        let mut content = Vec::new();

        let _ = self.loop_handle.insert_source(read_pipe, move |_, file, state| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.read(&mut reader_buffer) {
                    Ok(0) => {
                        let final_data = if is_text {
                            let text = String::from_utf8_lossy(&content);
                            let normalized = normalize_to_lf(text.into_owned());
                            normalized.into_bytes()
                        } else {
                            mem::take(&mut content)
                        };

                        let data = ClipboardData::new(mime.clone(), final_data);
                        let _ = state.reply_tx.send(Ok(Reply::Data(data)));
                        break PostAction::Remove;
                    },
                    Ok(n) => content.extend_from_slice(&reader_buffer[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(err) => {
                        let _ = state.reply_tx.send(Err(ClipboardError::Io(err)));
                        break PostAction::Remove;
                    },
                }
            }
        });

        Ok(())
    }

    /// Finish the DnD drop operation (accept the data).
    #[cfg(feature = "dnd")]
    pub fn finish_dnd(&mut self) {
        if let Some(offer_state) = self.current_drag_offer.take() {
            offer_state.offer.finish();
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.insert(seat.id(), Default::default());
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        let seat_state = self.seats.get_mut(&seat.id()).unwrap();

        match capability {
            Capability::Keyboard => {
                seat_state.keyboard = Some(seat.get_keyboard(qh, seat.id()));

                // Selection sources are tied to the keyboard, so add/remove decives
                // when we gain/loss capability.

                if seat_state.data_device.is_none() && self.data_device_manager_state.is_some() {
                    seat_state.data_device = self
                        .data_device_manager_state
                        .as_ref()
                        .map(|mgr| mgr.get_data_device(qh, &seat));
                }

                if seat_state.primary_device.is_none()
                    && self.primary_selection_manager_state.is_some()
                {
                    seat_state.primary_device = self
                        .primary_selection_manager_state
                        .as_ref()
                        .map(|mgr| mgr.get_selection_device(qh, &seat));
                }
            },
            Capability::Pointer => {
                seat_state.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            },
            _ => (),
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        let seat_state = self.seats.get_mut(&seat.id()).unwrap();
        match capability {
            Capability::Keyboard => {
                seat_state.data_device = None;
                seat_state.primary_device = None;

                if let Some(keyboard) = seat_state.keyboard.take() {
                    if keyboard.version() >= 3 {
                        keyboard.release()
                    }
                }
            },
            Capability::Pointer => {
                if let Some(pointer) = seat_state.pointer.take() {
                    if pointer.version() >= 3 {
                        pointer.release()
                    }
                }
            },
            _ => (),
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.remove(&seat.id());
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let seat = pointer.data::<PointerData>().unwrap().seat();
        let seat_id = seat.id();
        let seat_state = match self.seats.get_mut(&seat_id) {
            Some(seat_state) => seat_state,
            None => return,
        };

        let mut updated_serial = false;
        for event in events {
            match event.kind {
                PointerEventKind::Press { serial, .. }
                | PointerEventKind::Release { serial, .. } => {
                    updated_serial = true;
                    seat_state.latest_serial = serial;
                },
                _ => (),
            }
        }

        // Only update the seat we're using when the serial got updated.
        if updated_serial {
            self.latest_seat = Some(seat_id);
        }
    }
}

impl DataDeviceHandler for State {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        #[cfg(feature = "dnd")]
        {
            use sctk::data_device_manager::data_device::DataDeviceData;
            // Get the drag offer from the data device
            if let Some(offer) = data_device.data::<DataDeviceData>().and_then(|d| d.drag_offer()) {
                let mime_types: Vec<String> =
                    offer.with_mime_types(|mimes: &[String]| mimes.to_vec());

                // Store the drag offer state
                self.current_drag_offer = Some(DragOfferState {
                    offer: offer.clone(),
                    mime_types: mime_types.clone(),
                    x,
                    y,
                    surface: surface.clone(),
                    left: false,
                });

                // Dispatch enter event
                handle_dnd_enter(
                    &self.dnd_sender,
                    &mut self.dnd_destinations,
                    surface,
                    x,
                    y,
                    mime_types,
                );

                // Accept and set preferred action based on registered destination
                if let Some(rect) = self.dnd_destinations.find_rectangle(surface, x, y) {
                    // Find a compatible MIME type
                    if let Some(offer_state) = &self.current_drag_offer {
                        let compatible_mime = offer_state.mime_types.iter().find(|m| {
                            rect.mime_types.iter().any(|rm| m.as_str() == rm.as_str())
                        });
                        if let Some(mime) = compatible_mime {
                            offer_state.offer.accept_mime_type(offer_state.offer.serial, Some(mime.clone()));
                            offer_state.offer.set_actions(rect.actions, rect.preferred);
                        }
                    }
                }
            }
        }

        #[cfg(not(feature = "dnd"))]
        {
            let _ = (data_device, x, y, surface);
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        #[cfg(feature = "dnd")]
        {
            if let Some(ref mut offer_state) = self.current_drag_offer {
                offer_state.left = true;
            }
            handle_dnd_leave(&self.dnd_sender, &mut self.dnd_destinations);
        }
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _data_device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        #[cfg(feature = "dnd")]
        {
            if let Some(ref mut offer_state) = self.current_drag_offer {
                offer_state.x = x;
                offer_state.y = y;

                let surface = offer_state.surface.clone();
                handle_dnd_motion(&self.dnd_sender, &mut self.dnd_destinations, &surface, x, y);

                // Update accepted MIME type and actions based on new position
                if let Some(rect) = self.dnd_destinations.find_rectangle(&surface, x, y) {
                    let compatible_mime = offer_state.mime_types.iter().find(|m| {
                        rect.mime_types.iter().any(|rm| m.as_str() == rm.as_str())
                    });
                    if let Some(mime) = compatible_mime {
                        offer_state.offer.accept_mime_type(offer_state.offer.serial, Some(mime.clone()));
                        offer_state.offer.set_actions(rect.actions, rect.preferred);
                    } else {
                        offer_state.offer.accept_mime_type(offer_state.offer.serial, None);
                    }
                } else {
                    offer_state.offer.accept_mime_type(offer_state.offer.serial, None);
                }
            }
        }

        #[cfg(not(feature = "dnd"))]
        {
            let _ = (x, y);
        }
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        #[cfg(feature = "dnd")]
        {
            handle_dnd_drop(&self.dnd_sender, &self.dnd_destinations);
        }
    }

    // The selection is finished and ready to be used.
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        write_pipe: WritePipe,
    ) {
        // Check if this is a DnD source or a clipboard source
        #[cfg(feature = "dnd")]
        {
            if let Some(ref dnd_source) = self.dnd_source {
                if dnd_source.inner() == source {
                    // This is a DnD source - send the DnD data
                    if let Some(ref dnd_data) = self.dnd_source_data {
                        if dnd_data.mime_types.contains(&mime) {
                            // Mark FD as non-blocking
                            if set_non_blocking(write_pipe.as_raw_fd()).is_ok() {
                                let data: Rc<[u8]> = Rc::from(dnd_data.data.clone().into_boxed_slice());
                                let mut written = 0;
                                let _ = self.loop_handle.insert_source(
                                    write_pipe,
                                    move |_, file, _| {
                                        let file = unsafe { file.get_mut() };
                                        loop {
                                            match file.write(&data[written..]) {
                                                Ok(n) if written + n == data.len() => {
                                                    break PostAction::Remove;
                                                },
                                                Ok(n) => written += n,
                                                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                                                    break PostAction::Continue;
                                                },
                                                Err(_) => break PostAction::Remove,
                                            }
                                        }
                                    },
                                );
                            }
                        }
                    }
                    return;
                }
            }
        }
        // Otherwise handle as clipboard
        self.send_request(SelectionTarget::Clipboard, write_pipe, mime)
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, deleted: &WlDataSource) {
        self.data_sources.retain(|source| source.inner() != deleted);

        #[cfg(feature = "dnd")]
        {
            if let Some(ref dnd_source) = self.dnd_source {
                if dnd_source.inner() == deleted {
                    handle_source_cancelled(&self.dnd_sender);
                    self.dnd_source = None;
                    self.dnd_source_data = None;
                }
            }
        }
    }

    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _source: &WlDataSource,
        mime: Option<String>,
    ) {
        #[cfg(feature = "dnd")]
        {
            handle_source_mime(&self.dnd_sender, mime);
        }
        #[cfg(not(feature = "dnd"))]
        {
            let _ = mime;
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        #[cfg(feature = "dnd")]
        {
            handle_source_dropped(&self.dnd_sender);
        }
    }

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, action: DndAction) {
        #[cfg(feature = "dnd")]
        {
            handle_source_action(&self.dnd_sender, action);
        }
        #[cfg(not(feature = "dnd"))]
        {
            let _ = action;
        }
    }

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _source: &WlDataSource) {
        #[cfg(feature = "dnd")]
        {
            handle_source_finished(&self.dnd_sender);
            self.dnd_source = None;
            self.dnd_source_data = None;
        }
    }
}

impl DataOfferHandler for State {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
        // Source actions received - we could filter the actions we accept here
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        action: DndAction,
    ) {
        #[cfg(feature = "dnd")]
        {
            handle_dnd_selected_action(&self.dnd_sender, &self.dnd_destinations, action);
        }
        #[cfg(not(feature = "dnd"))]
        {
            let _ = action;
        }
    }
}

impl ProvidesRegistryState for State {
    registry_handlers![SeatState];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}

impl PrimarySelectionDeviceHandler for State {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl PrimarySelectionSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionSourceV1,
        mime: String,
        write_pipe: WritePipe,
    ) {
        self.send_request(SelectionTarget::Primary, write_pipe, mime);
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        deleted: &ZwpPrimarySelectionSourceV1,
    ) {
        self.primary_sources.retain(|source| source.inner() != deleted)
    }
}

impl Dispatch<WlKeyboard, ObjectId, State> for State {
    fn event(
        state: &mut State,
        _: &WlKeyboard,
        event: <WlKeyboard as sctk::reexports::client::Proxy>::Event,
        data: &ObjectId,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use sctk::reexports::client::protocol::wl_keyboard::Event as WlKeyboardEvent;
        let seat_state = match state.seats.get_mut(data) {
            Some(seat_state) => seat_state,
            None => return,
        };
        match event {
            WlKeyboardEvent::Key { serial, .. } | WlKeyboardEvent::Modifiers { serial, .. } => {
                seat_state.latest_serial = serial;
                state.latest_seat = Some(data.clone());
            },
            // NOTE both selections rely on keyboard focus.
            WlKeyboardEvent::Enter { serial, .. } => {
                seat_state.latest_serial = serial;
                seat_state.has_focus = true;
            },
            WlKeyboardEvent::Leave { .. } => {
                seat_state.latest_serial = 0;
                seat_state.has_focus = false;
            },
            _ => (),
        }
    }
}

delegate_seat!(State);
delegate_pointer!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
delegate_registry!(State);

#[derive(Debug, Clone, Copy)]
pub enum SelectionTarget {
    /// The target is clipboard selection.
    Clipboard,
    /// The target is primary selection.
    Primary,
}

#[derive(Debug, Default)]
struct ClipboardSeatState {
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    has_focus: bool,

    /// The latest serial used to set the selection content.
    latest_serial: u32,
}

impl Drop for ClipboardSeatState {
    fn drop(&mut self) {
        if let Some(keyboard) = self.keyboard.take() {
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }

        if let Some(pointer) = self.pointer.take() {
            if pointer.version() >= 3 {
                pointer.release();
            }
        }
    }
}

fn set_non_blocking(raw_fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };

    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}
