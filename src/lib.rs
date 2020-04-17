// FIXME - docs and exmaple with docs

// FIXME versions!!!
use std::ffi::c_void;
use std::io::Result as IoResult;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use sctk::reexports::client::protocol::wl_data_device_manager::WlDataDeviceManager;
use sctk::reexports::protocols::unstable::primary_selection::v1::client::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1;
use sctk::reexports::protocols::misc::gtk_primary_selection::client::gtk_primary_selection_device_manager::GtkPrimarySelectionDeviceManager;

use sctk::reexports::client::protocol::wl_keyboard::{Event as KeyboardEvent, WlKeyboard};
use sctk::reexports::client::protocol::wl_pointer::{Event as PointerEvent, WlPointer};
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::{Attached, DispatchData, Display, EventQueue};

use sctk::data_device::{
    DataDevice, DataDeviceHandler, DataDeviceHandling, DataSource, DataSourceEvent, DndEvent,
};
use sctk::environment::{Environment, SimpleGlobal};
use sctk::seat::keyboard::{self, RepeatKind};
use sctk::seat::{self, SeatData, SeatHandler, SeatHandling, SeatListener};

use std::os::unix::io::{FromRawFd, IntoRawFd};

use std::io::prelude::*;

mod mime;

use mime::MimeType;

struct SmithayClipboard {
    seats: SeatHandler,
    primary_selection: SimpleGlobal<ZwpPrimarySelectionDeviceManagerV1>,
    gtk_primary_selection: SimpleGlobal<GtkPrimarySelectionDeviceManager>,
    data_device_manager: sctk::data_device::DataDeviceHandler,
}

impl DataDeviceHandling for SmithayClipboard {
    fn set_callback<F>(&mut self, callback: F) -> Result<(), ()>
    where
        F: FnMut(WlSeat, DndEvent, DispatchData) + 'static,
    {
        self.data_device_manager.set_callback(callback)
    }

    fn with_device<F: FnOnce(&DataDevice)>(&self, seat: &WlSeat, f: F) -> Result<(), ()> {
        self.data_device_manager.with_device(seat, f)
    }
}

pub struct Clipboard {
    request_sender: Sender<ClipboardRequest>,
    request_receiver: Receiver<IoResult<String>>,
}

struct StoreRequestData {
    pub seat_name: Option<String>,
    pub contents: String,
}

impl StoreRequestData {
    fn new(seat_name: Option<String>, contents: String) -> Self {
        Self {
            seat_name,
            contents,
        }
    }
}

struct LoadRequestData {
    pub seat_name: Option<String>,
}

impl LoadRequestData {
    fn new(seat_name: Option<String>) -> Self {
        Self { seat_name }
    }
}

enum ClipboardRequest {
    Store(StoreRequestData),
    StorePrimary(StoreRequestData),
    Load(LoadRequestData),
    LoadPrimary(LoadRequestData),
    Exit,
}

impl ClipboardRequest {
    fn seat(&self) -> Option<String> {
        None
    }
}

impl Clipboard {
    /// Creates new clipboard which will be running on its own thread with its own event queue to
    /// handle clipboard requests.
    pub fn new(display: *mut c_void) -> Self {
        // XXX We should handle display very carefully and don't not drop it accidentaly, otherwise
        // will crash our client.
        let display = unsafe { Display::from_external_display(display as *mut _) };

        // Create channel to send data to clipboard thread
        let (request_sender, clipboard_request_receiver) = mpsc::channel();
        // Create channel to get data from the clipboard thread
        let (clipboard_reply_sender, request_receiver) = mpsc::channel();

        let _ = std::thread::Builder::new()
            .name(String::from("smithay-clipboard"))
            .spawn(move || {
                clipboard_thread(display, clipboard_request_receiver, clipboard_reply_sender);
            })
            .unwrap();

        Self {
            request_receiver,
            request_sender,
        }
    }

    /// Load clipboard data
    ///
    /// Loads content from a clipboard on a seat using the given `seat_name`. If `seat_name` is
    /// `None` it'll use the latest seat observed in pointer/keyboard events.
    pub fn load(&self, seat_name: Option<String>) -> IoResult<String> {
        let request = ClipboardRequest::Load(LoadRequestData::new(seat_name));
        let _ = self.request_sender.send(request);
        self.request_receiver.recv().unwrap()
    }

    /// Store to a clipboard
    ///
    /// Stores to a clipboard on a seat using the given `seat_name`. If `seat_name` is
    /// `None` it'll use the latest seat observed in pointer/keyboard events.
    pub fn store<T: Into<String>>(&self, seat_name: Option<String>, text: T) {
        let request = ClipboardRequest::Store(StoreRequestData::new(seat_name, text.into()));
        let _ = self.request_sender.send(request);
    }

    /// Load primary clipboard data
    ///
    /// Loads content from a  primary clipboard on a seat using the given `seat_name`. If
    /// `seat_name` is `None` it'll use the latest seat observed in pointer/keyboard events.
    pub fn load_primary(&self, seat_name: Option<String>) -> IoResult<String> {
        let request = ClipboardRequest::LoadPrimary(LoadRequestData::new(seat_name));
        let _ = self.request_sender.send(request);
        self.request_receiver.recv().unwrap()
    }

    /// Store to a primary clipboard
    ///
    /// Stores to a primary clipboard on a seat using the given `seat_name`. If `seat_name` is
    /// `None` it'll use the latest seat observed in pointer/keyboard events.
    pub fn store_primary<T: Into<String>>(&self, seat_name: Option<String>, text: T) {
        let request = ClipboardRequest::StorePrimary(StoreRequestData::new(seat_name, text.into()));
        let _ = self.request_sender.send(request);
    }
}

impl SmithayClipboard {
    fn new() -> Self {
        let mut seats = SeatHandler::new();
        let data_device_manager = DataDeviceHandler::init(&mut seats);
        Self {
            seats,
            primary_selection: SimpleGlobal::new(),
            gtk_primary_selection: SimpleGlobal::new(),
            data_device_manager,
        }
    }
}

impl SeatHandling for SmithayClipboard {
    fn listen<F: FnMut(Attached<WlSeat>, &SeatData, DispatchData) + 'static>(
        &mut self,
        f: F,
    ) -> SeatListener {
        self.seats.listen(f)
    }
}

sctk::environment!(SmithayClipboard,
    singles = [
    ZwpPrimarySelectionDeviceManagerV1 => primary_selection,
    GtkPrimarySelectionDeviceManager => gtk_primary_selection,
    sctk::reexports::client::protocol::wl_data_device_manager::WlDataDeviceManager => data_device_manager,
    ],
multis = [
    WlSeat => seats,
]
);

// TODO drop things properly, i.e. don't drop display on close

// TODO raname
struct Seat {
    pub seat: WlSeat,
    pub keyboard: Option<WlKeyboard>,
    pub pointer: Option<WlPointer>,
}

impl Seat {
    fn new(seat: WlSeat, keyboard: Option<WlKeyboard>, pointer: Option<WlPointer>) -> Self {
        Self {
            seat,
            keyboard,
            pointer,
        }
    }
}

#[derive(Default)]
struct ClipboardDispatchData {
    pub last_seat: Option<WlSeat>,
    pub last_serial: Option<u32>,
}

impl ClipboardDispatchData {
    fn new() -> Self {
        Self::default()
    }
}

/// Handle clipboard requests.
fn clipboard_thread(
    display: Display,
    request_recv: Receiver<ClipboardRequest>,
    clipboard_reply_sender: Sender<IoResult<String>>,
) {
    let mut queue = display.create_event_queue();
    let display_proxy = display.attach(queue.token());
    // Setup env with things we care about.
    let env = Environment::init(&display_proxy, SmithayClipboard::new());
    let req = queue.sync_roundtrip(&mut (), |_, _, _| unreachable!());
    let _ = req
        .and_then(|_| queue.sync_roundtrip(&mut (), |_, _, _| unreachable!()))
        .unwrap();

    // Check for primary selection providers
    let primary_selection = env.get_global::<ZwpPrimarySelectionDeviceManagerV1>();
    let gtk_primary_selection = env.get_global::<GtkPrimarySelectionDeviceManager>();

    // Just shutdown thread if global is not available?
    let data_device_manager = env.get_global::<WlDataDeviceManager>().unwrap();

    let mut seats = Vec::<Seat>::new();

    for seat in env.get_all_seats() {
        let seat_data = match seat::with_seat_data(&seat, |seat_data| seat_data.clone()) {
            Some(seat_data) => {
                // Handle defunct setas early on
                if seat_data.defunct {
                    seats.push(Seat::new(seat.detach(), None, None));
                    continue;
                }

                seat_data
            }
            _ => continue,
        };

        // Defunct was checked earlier, so try to bind keyboard and pointer

        // Bind keyboard
        let keyboard = if seat_data.has_keyboard {
            let keyboard = seat.get_keyboard();
            let seat_clone = seat.clone();

            keyboard.quick_assign(move |keyboard, event, dispatch_data| {
                keyboard_handler(seat_clone.detach(), event, dispatch_data);
            });

            Some(keyboard.detach())
        } else {
            None
        };

        // Bind poiter
        let pointer = if seat_data.has_pointer {
            let pointer = seat.get_pointer();
            let seat_clnoe = seat.clone();

            pointer.quick_assign(move |pointer, event, dispatch_data| {
                pointer_handler(seat_clnoe.detach(), event, dispatch_data);
            });

            Some(pointer.detach())
        } else {
            None
        };

        // Add new seat to tracker
        seats.push(Seat::new(seat.detach(), keyboard, pointer));
    }

    let _listener = env.listen_for_seats(move |seat, seat_data, _| {
        let detached_seat = seat.clone().detach();
        let pos = seats.iter().position(|st| st.seat == detached_seat);
        let index = pos.unwrap_or_else(|| {
            seats.push(Seat::new(detached_seat, None, None));
            seats.len() - 1
        });

        let seat_resources = &mut seats[index];

        if seat_data.has_keyboard && !seat_data.defunct {
            if seat_resources.keyboard.is_none() {
                let keyboard = seat.get_keyboard();
                let seat_clone = seat.clone();

                keyboard.quick_assign(move |keyboard, event, dispatch_data| {
                    keyboard_handler(seat_clone.detach(), event, dispatch_data);
                });

                seat_resources.keyboard = Some(keyboard.detach());
            }
        } else {
            // We've removed keyboard capabitily, clean up
            if let Some(keyboard) = seat_resources.keyboard.take() {
                keyboard.release();
            }
        }

        if seat_data.has_pointer && !seat_data.defunct {
            if seat_resources.pointer.is_none() {
                let pointer = seat.get_pointer();
                let seat_clone = seat.clone();

                pointer.quick_assign(move |pointer, event, dispatch_data| {
                    pointer_handler(seat_clone.detach(), event, dispatch_data);
                });

                seat_resources.pointer = Some(pointer.detach());
            }
        } else {
            // We've removed pointer capabitily, clean up
            if let Some(pointer) = seat_resources.pointer.take() {
                pointer.release();
            }
        }
    });

    // Flush display
    let _ = queue.display().flush();

    // Data to track latest seat
    let mut dispatch_data = ClipboardDispatchData::new();

    // FIXME - we should use select(3) and friends and just have 2 events sources, one from users
    // and one is wayland queue, so we can get rid of this heuristic based logic with sleeps and
    // wakeups.

    // We should provide lower sleep amounts in a moments of spaming our clipboard
    let mut sleep_amount = 0;

    // Provide our clipboard a warm start, so 16 initial cycles will be at 1ms and other will go
    // like 1 2 4 8 16 32 50 50 and so on
    let mut warm_start_amount = 0;
    // FIXME UTF8_STRING mime type and friends.

    // Flush display
    let _ = queue.display().flush();

    loop {
        if let Ok(request) = request_recv.try_recv() {
            // Lower sleep amount to zero, so the next recv dispatch of the event queue and recv
            // will be instant.
            sleep_amount = 0;

            let req = queue.sync_roundtrip(&mut dispatch_data, |_, _, _| unreachable!());

            // Notify back that we have nothing to do.
            let (seat, serial) = match (
                dispatch_data.last_seat.as_ref(),
                dispatch_data.last_serial.as_ref(),
            ) {
                (Some(seat), Some(serial)) => (seat.clone(), serial.clone()),
                _ => continue,
            };

            // FIXME - get seat name from the request

            match request {
                ClipboardRequest::Load(_) => {
                    env.with_data_device(&seat, |device| {
                        let (mut reader, mime_type) = match device.with_selection(|offer| {
                            // Check that we have offer
                            let offer = match offer {
                                Some(offer) => offer,
                                None => return None,
                            };

                            // Check that we can work with requested mime type and pick the one
                            // that suits us more
                            let mime_type = match offer.with_mime_types(MimeType::find_allowed) {
                                Some(mime_type) => mime_type,
                                None => return None,
                            };

                            // Request given mime type
                            let reader = offer.receive(mime_type.to_string()).unwrap();
                            Some((reader, mime_type))
                        }) {
                            Some((reader, mime_type)) => (reader, mime_type),
                            None => return (),
                        };

                        let _ = queue.sync_roundtrip(&mut dispatch_data, |_, _, _| unreachable!());

                        let mut contents = String::new();
                        let result = reader.read_to_string(&mut contents).map(|_| contents);

                        clipboard_reply_sender.send(result).unwrap();
                    })
                    .unwrap();
                }
                ClipboardRequest::LoadPrimary(_) => {}
                ClipboardRequest::Store(store_data) => {
                    let contents = store_data.contents.clone();
                    let data_source = DataSource::new(
                        &data_device_manager,
                        [MimeType::TextPlainUtf8.to_string()].into_iter(),
                        move |event, _| match event {
                            DataSourceEvent::Send { mut pipe, .. } => {
                                write!(pipe, "{}", contents).unwrap();
                            }
                            _ => (),
                        },
                    );

                    env.with_data_device(&seat, |device| {
                        device.set_selection(&Some(data_source), serial);

                        let _ = queue.sync_roundtrip(&mut dispatch_data, |_, _, _| unreachable!());
                    });
                }
                ClipboardRequest::StorePrimary(_) => {}
                ClipboardRequest::Exit => break,
            }
        }

        let pending_events = queue.dispatch_pending(&mut dispatch_data, |_, _, _| {});
        std::thread::sleep(std::time::Duration::from_millis(sleep_amount));
    }
}

// FIXME
fn pointer_handler(seat: WlSeat, event: PointerEvent, mut dispatch_data: DispatchData) {
    let dispatch_data = dispatch_data.get::<ClipboardDispatchData>().unwrap();
    dispatch_data.last_seat = Some(seat);
    match event {
        PointerEvent::Enter { serial, .. } => {
            dispatch_data.last_serial = Some(serial);
        }
        PointerEvent::Button { serial, .. } => {
            dispatch_data.last_serial = Some(serial);
        }
        _ => {}
    }
}

// FIXME
fn keyboard_handler(seat: WlSeat, event: KeyboardEvent, mut dispatch_data: DispatchData) {
    let dispatch_data = dispatch_data.get::<ClipboardDispatchData>().unwrap();
    dispatch_data.last_seat = Some(seat);
    match event {
        KeyboardEvent::Enter { serial, .. } => {
            dispatch_data.last_serial = Some(serial);
        }
        KeyboardEvent::Key { serial, .. } => {
            dispatch_data.last_serial = Some(serial);
        }
        KeyboardEvent::Leave { serial, .. } => {
            dispatch_data.last_serial = Some(serial);
        }
        _ => {}
    }
}
