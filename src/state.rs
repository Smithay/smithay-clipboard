use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::marker::PhantomData;
use std::mem;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::mpsc::Sender;

use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferError, DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use sctk::data_device_manager::{DataDeviceManagerState, WritePipe};
use sctk::output::{OutputHandler, OutputState};
use sctk::primary_selection::device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler};
use sctk::primary_selection::selection::{PrimarySelectionSource, PrimarySelectionSourceHandler};
use sctk::primary_selection::PrimarySelectionManagerState;
use sctk::reexports::client::protocol::wl_output::WlOutput;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::pointer::{PointerData, PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shm::multi::MultiPool;
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_pointer,
    delegate_primary_selection, delegate_registry, delegate_seat, delegate_shm, registry_handlers,
};

use sctk::reexports::calloop::{LoopHandle, PostAction};
use sctk::reexports::client::globals::GlobalList;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_keyboard::WlKeyboard;
use sctk::reexports::client::protocol::wl_pointer::WlPointer;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};
use wayland_backend::client::ObjectId;

use crate::dnd::state::DndState;
use crate::dnd::{DndEvent, DndSurface};
use crate::mime::{AsMimeTypes, MimeType};
use crate::text::Text;

pub struct State<T> {
    pub primary_selection_manager_state: Option<PrimarySelectionManagerState>,
    pub data_device_manager_state: Option<DataDeviceManagerState>,
    pub reply_tx: Sender<Result<(Vec<u8>, MimeType)>>,
    pub exit: bool,

    registry_state: RegistryState,
    pub(crate) seat_state: SeatState,

    pub(crate) seats: HashMap<ObjectId, ClipboardSeatState>,
    /// The latest seat which got an event.
    pub(crate) latest_seat: Option<ObjectId>,

    pub(crate) loop_handle: LoopHandle<'static, Self>,
    pub(crate) queue_handle: QueueHandle<Self>,

    primary_sources: Vec<PrimarySelectionSource>,
    primary_selection_content: Box<dyn AsMimeTypes>,
    primary_selection_mime_types: Rc<Cow<'static, [MimeType]>>,

    data_sources: Vec<CopyPasteSource>,
    data_selection_content: Box<dyn AsMimeTypes>,
    data_selection_mime_types: Rc<Cow<'static, [MimeType]>>,
    #[cfg(feature = "dnd")]
    pub(crate) dnd_state: crate::dnd::state::DndState<T>,
    pub(crate) compositor_state: CompositorState,
    output_state: OutputState,
    pub(crate) shm: Shm,
    pub(crate) pool: MultiPool<u8>,
    _phantom: PhantomData<T>,
}

impl<T: 'static + Clone> State<T> {
    #[must_use]
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<Self>,
        loop_handle: LoopHandle<'static, Self>,
        reply_tx: Sender<Result<(Vec<u8>, MimeType)>>,
    ) -> Option<Self> {
        let mut seats = HashMap::new();

        let data_device_manager_state = DataDeviceManagerState::bind(globals, queue_handle).ok();
        let primary_selection_manager_state =
            PrimarySelectionManagerState::bind(globals, queue_handle).ok();

        // When both globals are not available nothing could be done.
        if data_device_manager_state.is_none() && primary_selection_manager_state.is_none() {
            return None;
        }

        let compositor_state =
            CompositorState::bind(globals, queue_handle).expect("wl_compositor not available");
        let output_state = OutputState::new(globals, queue_handle);
        let shm = Shm::bind(globals, queue_handle).expect("wl_shm not available");

        let seat_state = SeatState::new(globals, queue_handle);
        for seat in seat_state.seats() {
            seats.insert(seat.id(), Default::default());
        }

        Some(Self {
            registry_state: RegistryState::new(globals),
            primary_selection_content: Box::new(Text(String::new())),
            data_selection_content: Box::new(Text(String::new())),
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
            primary_selection_mime_types: Rc::new(Default::default()),
            data_selection_mime_types: Rc::new(Default::default()),
            #[cfg(feature = "dnd")]
            dnd_state: DndState::default(),
            _phantom: PhantomData,
            compositor_state,
            output_state,
            pool: MultiPool::new(&shm).expect("Failed to create memory pool."),
            shm,
        })
    }

    /// Store selection for the given target.
    ///
    /// Selection source is only created when `Some(())` is returned.
    pub fn store_selection(&mut self, ty: Target, contents: Box<dyn AsMimeTypes>) -> Option<()> {
        let latest = self.latest_seat.as_ref()?;
        let seat = self.seats.get_mut(latest)?;

        if !seat.has_focus {
            return None;
        }

        match ty {
            Target::Clipboard => {
                let mgr = self.data_device_manager_state.as_ref()?;
                let mime_types = contents.available();
                self.data_selection_content = contents;
                let source = mgr.create_copy_paste_source(&self.queue_handle, mime_types.iter());
                self.data_selection_mime_types = Rc::new(mime_types);
                source.set_selection(seat.data_device.as_ref().unwrap(), seat.latest_serial);
                self.data_sources.push(source);
            },
            Target::Primary => {
                let mgr = self.primary_selection_manager_state.as_ref()?;
                let mime_types = contents.available();
                self.primary_selection_content = contents;
                let source = mgr.create_selection_source(&self.queue_handle, mime_types.iter());
                self.primary_selection_mime_types = Rc::new(mime_types);
                source.set_selection(seat.primary_device.as_ref().unwrap(), seat.latest_serial);
                self.primary_sources.push(source);
            },
        }

        Some(())
    }

    /// Load data for the given target.
    pub fn load(&mut self, ty: Target, allowed_mime_types: &[MimeType]) -> Result<()> {
        let latest = self
            .latest_seat
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Other, "no events received on any seat"))?;
        let seat = self
            .seats
            .get_mut(latest)
            .ok_or_else(|| Error::new(ErrorKind::Other, "active seat lost"))?;

        if !seat.has_focus {
            return Err(Error::new(ErrorKind::Other, "client doesn't have focus"));
        }

        let (read_pipe, mut mime_type) = match ty {
            Target::Clipboard => {
                let selection = seat
                    .data_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or_else(|| Error::new(ErrorKind::Other, "selection is empty"))?;

                let mime_type = selection
                    .with_mime_types(|offered| MimeType::find_allowed(offered, allowed_mime_types))
                    .ok_or_else(|| {
                        Error::new(ErrorKind::NotFound, "supported mime-type is not found")
                    })?;

                (
                    selection.receive(mime_type.to_string()).map_err(|err| match err {
                        DataOfferError::InvalidReceive => {
                            Error::new(ErrorKind::Other, "offer is not ready yet")
                        },
                        DataOfferError::Io(err) => err,
                    })?,
                    mime_type,
                )
            },
            Target::Primary => {
                let selection = seat
                    .primary_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or_else(|| Error::new(ErrorKind::Other, "selection is empty"))?;

                let mime_type = selection
                    .with_mime_types(|offered| MimeType::find_allowed(offered, allowed_mime_types))
                    .ok_or_else(|| {
                        Error::new(ErrorKind::NotFound, "supported mime-type is not found")
                    })?;

                (selection.receive(mime_type.to_string())?, mime_type)
            },
        };

        // Mark FD as non-blocking so we won't block ourselves.
        unsafe {
            set_non_blocking(read_pipe.as_raw_fd())?;
        }

        let mut reader_buffer = [0; 4096];
        let mut content = Vec::new();
        let _ = self.loop_handle.insert_source(read_pipe, move |_, file, state| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.read(&mut reader_buffer) {
                    Ok(0) => {
                        let _ = state
                            .reply_tx
                            .send(Ok((mem::take(&mut content), mem::take(&mut mime_type))));
                        break PostAction::Remove;
                    },
                    Ok(n) => content.extend_from_slice(&reader_buffer[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(err) => {
                        let _ = state.reply_tx.send(Err(err));
                        break PostAction::Remove;
                    },
                };
            }
        });

        Ok(())
    }

    fn send_request(&mut self, ty: Target, write_pipe: WritePipe, mime: String) {
        let Some(mime_type) = MimeType::find_allowed(&[mime], match ty {
            Target::Clipboard => &self.data_selection_mime_types,
            Target::Primary => &self.primary_selection_mime_types,
        }) else {
            return;
        };

        // Mark FD as non-blocking so we won't block ourselves.
        unsafe {
            if set_non_blocking(write_pipe.as_raw_fd()).is_err() {
                return;
            }
        }

        // Don't access the content on the state directly, since it could change during
        // the send.
        let contents = match ty {
            Target::Clipboard => self.data_selection_content.as_bytes(&mime_type),
            Target::Primary => self.primary_selection_content.as_bytes(&mime_type),
        };

        let Some(contents) = contents else {
            return;
        };

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
}

impl<T: 'static + Clone> SeatHandler for State<T> {
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

impl<T: 'static + Clone> PointerHandler for State<T> {
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

impl<T: 'static + Clone> DataDeviceHandler for State<T>
where
    DndSurface<T>: Clone,
{
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        wl_data_device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        #[cfg(feature = "dnd")]
        self.offer_enter(x, y, surface, wl_data_device);
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        #[cfg(feature = "dnd")]
        self.offer_leave();
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        wl_data_device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        #[cfg(feature = "dnd")]
        self.offer_motion(x, y, wl_data_device);
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, d: &WlDataDevice) {
        #[cfg(feature = "dnd")]
        self.offer_drop(d)
    }

    // The selection is finished and ready to be used.
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl<T: 'static + Clone> DataSourceHandler for State<T> {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _source: &WlDataSource,
        mime: String,
        write_pipe: WritePipe,
    ) {
        #[cfg(feature = "dnd")]
        if self
            .dnd_state
            .dnd_source
            .as_ref()
            .map(|my_source| my_source.inner() == _source)
            .unwrap_or_default()
        {
            self.send_dnd_request(write_pipe, mime);
            return;
        }
        self.send_request(Target::Clipboard, write_pipe, mime)
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, deleted: &WlDataSource) {
        self.data_sources.retain(|source| source.inner() != deleted);
        #[cfg(feature = "dnd")]
        {
            self.dnd_state.source_content = None;
            self.dnd_state.dnd_source = None;
            if let Some(s) = self.dnd_state.sender.as_ref() {
                _ = s.send(DndEvent::Source(crate::dnd::SourceEvent::Cancelled));
            }
            _ = self.pool.remove(&0);
            self.dnd_state.icon_surface = None;
        }
    }

    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        m: Option<String>,
    ) {
        #[cfg(feature = "dnd")]
        {
            if let Some(s) = self.dnd_state.sender.as_ref() {
                _ = s.send(DndEvent::Source(crate::dnd::SourceEvent::Mime(
                    m.map(|s| MimeType::from(Cow::Owned(s))),
                )));
            }
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        #[cfg(feature = "dnd")]
        {
            if let Some(s) = self.dnd_state.sender.as_ref() {
                _ = s.send(DndEvent::Source(crate::dnd::SourceEvent::Dropped))
            }
            _ = self.pool.remove(&0);
            self.dnd_state.icon_surface = None;
        }
    }

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, a: DndAction) {
        #[cfg(feature = "dnd")]
        {
            if let Some(s) = self.dnd_state.sender.as_ref() {
                _ = s.send(DndEvent::Source(crate::dnd::SourceEvent::Action(a)))
            }
        }
    }

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        #[cfg(feature = "dnd")]
        {
            self.dnd_state.source_content = None;
            self.dnd_state.dnd_source = None;
            if let Some(s) = self.dnd_state.sender.as_ref() {
                _ = s.send(DndEvent::Source(crate::dnd::SourceEvent::Finished));
            }
        }
    }
}

impl<T: 'static + Clone> DataOfferHandler for State<T> {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        action: DndAction,
    ) {
        #[cfg(feature = "dnd")]
        self.dnd_state.selected_action(action);
    }
}

impl<T: 'static + Clone> ProvidesRegistryState for State<T> {
    registry_handlers![SeatState];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}

impl<T: 'static + Clone> PrimarySelectionDeviceHandler for State<T> {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl<T: 'static + Clone> PrimarySelectionSourceHandler for State<T> {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionSourceV1,
        mime: String,
        write_pipe: WritePipe,
    ) {
        self.send_request(Target::Primary, write_pipe, mime);
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

impl<T: 'static + Clone> Dispatch<WlKeyboard, ObjectId, State<T>> for State<T> {
    fn event(
        state: &mut State<T>,
        _: &WlKeyboard,
        event: <WlKeyboard as sctk::reexports::client::Proxy>::Event,
        data: &ObjectId,
        _: &Connection,
        _: &QueueHandle<State<T>>,
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

impl<T: 'static + Clone> CompositorHandler for State<T> {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &sctk::reexports::client::protocol::wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &sctk::reexports::client::protocol::wl_surface::WlSurface,
        _new_transform: sctk::reexports::client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &sctk::reexports::client::protocol::wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl<T: 'static + Clone> OutputHandler for State<T> {
    fn output_state(&mut self) -> &mut sctk::output::OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: sctk::reexports::client::protocol::wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: sctk::reexports::client::protocol::wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: sctk::reexports::client::protocol::wl_output::WlOutput,
    ) {
    }
}

impl<T: 'static + Clone> ShmHandler for State<T> {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(@<T: 'static + Clone> State<T>);
delegate_output!(@<T: 'static + Clone> State<T>);
delegate_shm!(@<T: 'static + Clone> State<T>);
delegate_seat!(@<T: 'static + Clone> State<T>);
delegate_pointer!(@<T: 'static + Clone> State<T>);
delegate_data_device!(@<T: 'static + Clone> State<T>);
delegate_primary_selection!(@<T: 'static + Clone> State<T>);
delegate_registry!(@<T: 'static + Clone> State<T>);

#[derive(Debug, Clone, Copy)]
pub enum Target {
    /// The target is clipboard selection.
    Clipboard,
    /// The target is primary selection.
    Primary,
}

#[derive(Debug, Default)]
pub(crate) struct ClipboardSeatState {
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    pub(crate) data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    pub(crate) has_focus: bool,

    /// The latest serial used to set the selection content.
    pub(crate) latest_serial: u32,
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

pub(crate) unsafe fn set_non_blocking(raw_fd: RawFd) -> std::io::Result<()> {
    let flags = libc::fcntl(raw_fd, libc::F_GETFL);

    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}
