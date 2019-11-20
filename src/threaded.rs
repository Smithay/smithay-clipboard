use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Deref;
use std::os::unix::io::FromRawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nix::fcntl::OFlag;
use nix::unistd::{close, pipe2};

use sctk::data_device::{DataDevice, DataSource, DataSourceEvent};
use sctk::keyboard::{map_keyboard_auto, Event as KbEvent};
use sctk::reexports::client::protocol::{
    wl_data_device_manager, wl_display::WlDisplay, wl_pointer::Event as PtrEvent, wl_registry,
    wl_seat,
};
use sctk::reexports::client::{Display, EventQueue, GlobalEvent, GlobalManager, NewProxy};
use sctk::reexports::protocols::misc::gtk_primary_selection::client::{
    gtk_primary_selection_device::Event as GtkPrimarySelectionDeviceEvent,
    gtk_primary_selection_device::GtkPrimarySelectionDevice,
    gtk_primary_selection_device_manager::GtkPrimarySelectionDeviceManager,
    gtk_primary_selection_offer::GtkPrimarySelectionOffer, gtk_primary_selection_source,
};
use sctk::reexports::protocols::unstable::primary_selection::v1::client::{
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1 as PrimarySelectionDeviceMgr,
    zwp_primary_selection_device_v1::{
        Event as ZwpPrimarySelectionDeviceEvent,
        ZwpPrimarySelectionDeviceV1 as PrimarySelectionDevice,
    },
    zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1 as PrimarySelectionOffer,
    zwp_primary_selection_source_v1,
};
use sctk::wayland_client::sys::client::wl_display;

/// Used to store registered seats and their last event serial
type SeatMap = HashMap<
    String,
    (
        Arc<Mutex<DataDevice>>,
        u32,
        Arc<Mutex<Option<PrimarySelectionDevice>>>,
        Arc<Mutex<Option<PrimarySelectionOffer>>>,
        Arc<Mutex<Option<GtkPrimarySelectionDevice>>>,
        Arc<Mutex<Option<GtkPrimarySelectionOffer>>>,
    ),
>;

/// Object representing the Wayland clipboard
pub struct ThreadedClipboard {
    request_send: mpsc::Sender<ThreadRequest>,
    load_recv: mpsc::Receiver<String>,
}

// Kill thread when clipboard object is dropped
impl Drop for ThreadedClipboard {
    fn drop(&mut self) {
        self.request_send.send(ThreadRequest::Kill).unwrap()
    }
}

impl ThreadedClipboard {
    /// Creates a new wayland clipboard object
    ///
    /// Spawns a new thread to dispatch messages to the wayland server every
    /// 50ms to ensure the server can read stored data
    pub fn new(display: &Display) -> Self {
        let (request_send, request_recv) = mpsc::channel();
        let (load_send, load_recv) = mpsc::channel();
        let display = display.clone();

        // Spawn a thread to handle the clipboard as regular dispatching of the wayland thread is needed
        std::thread::spawn(move || {
            let mut event_queue = display.create_event_queue();
            let display = (*display)
                .as_ref()
                .make_wrapper(&event_queue.get_token())
                .unwrap();
            clipboard_thread(&display, &mut event_queue, request_recv, load_send);
        });

        ThreadedClipboard {
            request_send,
            load_recv,
        }
    }

    /// Creates a new wayland clipboard object from a mutable `wl_display` ptr
    ///
    /// Spawns a new thread to dispatch messages to the wayland server every
    /// 50ms to ensure the server can read stored data
    pub unsafe fn new_from_external(display_ptr: *mut wl_display) -> Self {
        let (request_send, request_recv) = mpsc::channel();
        let (load_send, load_recv) = mpsc::channel();
        let display = display_ptr.as_mut().unwrap();

        // Spawn a thread to handle the clipboard as regular dispatching of the wayland thread is needed
        std::thread::spawn(move || {
            let (display, mut event_queue) = Display::from_external_display(display);
            clipboard_thread(&display, &mut event_queue, request_recv, load_send);
        });

        ThreadedClipboard {
            request_send,
            load_recv,
        }
    }

    /// Returns text from the wayland clipboard
    ///
    /// If provided with a seat name that seat must be in
    /// focus to work. Otherwise if no seat name is provided
    /// the name of the seat to last generate a key or pointer event
    /// is used
    pub fn load(&mut self, seat_name: Option<String>) -> String {
        self.request_send
            .send(ThreadRequest::Load(seat_name))
            .unwrap();
        self.load_recv.recv().unwrap()
    }

    /// Stores text in the wayland clipboard
    ///
    /// If provided with a seat name that seat must be in
    /// focus to work. Otherwise if no seat name is provided
    /// the name of the seat to last generate a key or pointer event
    /// is used
    pub fn store<T>(&mut self, seat_name: Option<String>, text: T)
    where
        T: Into<String>,
    {
        self.request_send
            .send(ThreadRequest::Store(seat_name, text.into()))
            .unwrap()
    }

    /// Returns text from the primary selection of the wayland clipboard
    ///
    /// If provided with a seat name that seat must be in
    /// focus to work. Otherwise if no seat name is provided
    /// the name of the seat to last generate a key or pointer event
    /// is used
    pub fn load_primary(&mut self, seat_name: Option<String>) -> String {
        self.request_send
            .send(ThreadRequest::LoadPrimary(seat_name))
            .unwrap();
        self.load_recv.recv().unwrap()
    }

    /// Stores text in the primary selection of the wayland clipboard
    ///
    /// If provided with a seat name that seat must be in
    /// focus to work. Otherwise if no seat name is provided
    /// the name of the seat to last generate a key or pointer event
    /// is used
    pub fn store_primary(&mut self, seat_name: Option<String>, text: String) {
        self.request_send
            .send(ThreadRequest::StorePrimary(seat_name, text))
            .unwrap()
    }
}

/// Requests sent to the clipboard thread
enum ThreadRequest {
    /// Store text in a specific seats clipboard
    Store(Option<String>, String),
    /// Load text from a specific seats clipboard
    Load(Option<String>),
    /// Store text in a specific seats primary clipboard
    StorePrimary(Option<String>, String),
    /// Load text in a specific seats primary clipboard
    LoadPrimary(Option<String>),
    /// Kill the thread
    Kill,
}

/// Handles the setup and running of the clipboard thread
fn clipboard_thread(
    display: &WlDisplay,
    event_queue: &mut EventQueue,
    request_recv: mpsc::Receiver<ThreadRequest>,
    load_send: mpsc::Sender<String>,
) {
    // Create a seat map to register seats
    let seat_map = Arc::new(Mutex::new(SeatMap::new()));

    // Store unimplemented seats so we can implement them when the data device manager is implemented
    let data_device_manager = Arc::new(Mutex::new(None));
    let mut unimplemented_seats = Vec::new();

    let primary_selection_device_manager = Arc::new(Mutex::new(None));
    let gtk_primary_selection_device_manager = Arc::new(Mutex::new(None));

    // Store the name of the seat that last sends an event for use as the default seat
    let last_seat_name = Arc::new(Mutex::new(String::new()));

    let data_device_manager_clone = data_device_manager.clone();
    let primary_selection_device_manager_clone = primary_selection_device_manager.clone();
    let gtk_primary_selection_device_manager_clone = gtk_primary_selection_device_manager.clone();
    let seat_map_clone = seat_map.clone();
    let last_seat_name_clone = last_seat_name.clone();

    // Register wl_seat objects and wl_data_device_manager
    GlobalManager::new_with_cb(&display, move |event, reg| {
        if let GlobalEvent::New {
            id,
            ref interface,
            version,
        } = event
        {
            if "wl_seat" == interface.as_str() && version >= 2 {
                if let Some(ref data_device_manager) =
                    data_device_manager_clone.lock().unwrap().deref()
                {
                    // Implement the seat
                    implement_seat(
                        id,
                        version,
                        seat_map_clone.clone(),
                        last_seat_name_clone.clone(),
                        data_device_manager,
                        &reg,
                        primary_selection_device_manager_clone.clone(),
                        gtk_primary_selection_device_manager_clone.clone(),
                    );
                } else {
                    // Store the seat for implementation once wl_data_device_manager is registered
                    unimplemented_seats.push((id, version));
                }
            } else if "wl_data_device_manager" == interface.as_str() {
                // Register the wl_data_device_manager
                *data_device_manager_clone.lock().unwrap() = Some(
                    reg.bind::<wl_data_device_manager::WlDataDeviceManager, _>(
                        version,
                        id,
                        NewProxy::implement_dummy,
                    )
                    .unwrap(),
                );
                // Implement the unimplemented seats
                for (id, version) in &unimplemented_seats {
                    implement_seat(
                        *id,
                        *version,
                        seat_map_clone.clone(),
                        last_seat_name_clone.clone(),
                        data_device_manager_clone.lock().unwrap().as_ref().unwrap(),
                        &reg,
                        primary_selection_device_manager_clone.clone(),
                        gtk_primary_selection_device_manager_clone.clone(),
                    );
                }
            } else if "zwp_primary_selection_device_manager_v1" == interface.as_str() {
                // Register the zwp_primary_selection_device_manager
                *primary_selection_device_manager_clone.lock().unwrap() = Some(
                    reg.bind::<PrimarySelectionDeviceMgr, _>(
                        version,
                        id,
                        NewProxy::implement_dummy,
                    )
                    .unwrap(),
                );
            } else if "gtk_primary_selection_device_manager" == interface.as_str() {
                *gtk_primary_selection_device_manager_clone.lock().unwrap() = Some(
                    reg.bind::<GtkPrimarySelectionDeviceManager, _>(
                        version,
                        id,
                        NewProxy::implement_dummy,
                    )
                    .unwrap(),
                );
            }
        }
    });
    event_queue.sync_roundtrip().unwrap();

    // We should provide lower sleep amounts in a moments of spaming our clipboard
    let mut sleep_amount = 50;
    // Provide our clipboard a warm start, so 16 initial cycles will be at 1ms and other will go
    // like 1 2 4 8 16 32 50 50 and so on
    let mut warm_start_amount = 0;

    // Thread loop to handle requests and dispatch the event queue
    loop {
        if let Ok(request) = request_recv.try_recv() {
            // Lower sleep amount to zero, so the next recv will be instant
            sleep_amount = 0;

            match request {
                // Load text from clipboard
                ThreadRequest::Load(seat_name) => {
                    event_queue.sync_roundtrip().unwrap();
                    let seat_map = seat_map.lock().unwrap().clone();

                    // Get the clipboard contents of the requested seat from the seat map
                    let contents = seat_map
                        .get(&seat_name.unwrap_or_else(|| last_seat_name.lock().unwrap().clone()))
                        .map_or(String::new(), |seat| {
                            let mut reader = None;
                            seat.0.lock().unwrap().with_selection(|offer| {
                                if let Some(offer) = offer {
                                    offer.with_mime_types(|types| {
                                        if types.contains(&"text/plain;charset=utf-8".to_string()) {
                                            reader = Some(
                                                offer
                                                    .receive("text/plain;charset=utf-8".into())
                                                    .unwrap(),
                                            );
                                        }
                                    });
                                }
                            });
                            event_queue.sync_roundtrip().unwrap();
                            reader.map_or(String::new(), |mut reader| {
                                let mut contents = String::new();
                                reader.read_to_string(&mut contents).unwrap();
                                contents
                            })
                        });
                    // Normalization should happen only on `text/plain;charset=utf-8`, in case we
                    // add other mime types consult gtk for normalization.
                    let contents = normilize_to_lf(contents);
                    load_send.send(contents).unwrap();
                }
                // Store text in the clipboard
                ThreadRequest::Store(seat_name, contents) => {
                    event_queue.sync_roundtrip().unwrap();
                    let seat_map = seat_map.lock().unwrap().clone();

                    // Get the requested seat from the seat map
                    if let Some((device, enter_serial, _, _, _, _)) = seat_map
                        .get(&seat_name.unwrap_or_else(|| last_seat_name.lock().unwrap().clone()))
                    {
                        let data_source = DataSource::new(
                            data_device_manager.lock().unwrap().as_ref().unwrap(),
                            &["text/plain;charset=utf-8"],
                            move |source_event| {
                                if let DataSourceEvent::Send { mut pipe, .. } = source_event {
                                    write!(pipe, "{}", contents).unwrap();
                                }
                            },
                        );
                        device
                            .lock()
                            .unwrap()
                            .set_selection(&Some(data_source), *enter_serial);

                        event_queue.sync_roundtrip().unwrap();
                    }
                }
                // Load text from primary clipboard
                ThreadRequest::LoadPrimary(seat_name) => {
                    event_queue.sync_roundtrip().unwrap();
                    let seat_map = seat_map.lock().unwrap().clone();

                    // Get the primary clipboard contents of the requested seat from the seat map
                    let contents = if primary_selection_device_manager.lock().unwrap().is_some() {
                        seat_map
                            .get(
                                &seat_name
                                    .unwrap_or_else(|| last_seat_name.lock().unwrap().clone()),
                            )
                            .map_or(String::new(), |seat| {
                                seat.3.lock().unwrap().as_ref().map_or(
                                    String::new(),
                                    |primary_offer| {
                                        let (readfd, writefd) = pipe2(OFlag::O_CLOEXEC).unwrap();
                                        let mut file =
                                            unsafe { std::fs::File::from_raw_fd(readfd) };
                                        primary_offer.receive(
                                            "text/plain;charset=utf-8".to_string(),
                                            writefd,
                                        );
                                        close(writefd).unwrap();
                                        let mut contents = String::new();
                                        event_queue.sync_roundtrip().unwrap();
                                        file.read_to_string(&mut contents).unwrap();
                                        contents
                                    },
                                )
                            })
                    } else if gtk_primary_selection_device_manager
                        .lock()
                        .unwrap()
                        .is_some()
                    {
                        seat_map
                            .get(
                                &seat_name
                                    .unwrap_or_else(|| last_seat_name.lock().unwrap().clone()),
                            )
                            .map_or(String::new(), |seat| {
                                seat.5.lock().unwrap().as_ref().map_or(
                                    String::new(),
                                    |primary_offer| {
                                        let (readfd, writefd) = pipe2(OFlag::O_CLOEXEC).unwrap();
                                        let mut file =
                                            unsafe { std::fs::File::from_raw_fd(readfd) };
                                        primary_offer.receive(
                                            "text/plain;charset=utf-8".to_string(),
                                            writefd,
                                        );
                                        close(writefd).unwrap();
                                        let mut contents = String::new();
                                        event_queue.sync_roundtrip().unwrap();
                                        file.read_to_string(&mut contents).unwrap();
                                        contents
                                    },
                                )
                            })
                    } else {
                        String::new()
                    };
                    // Normalization should happen only on `text/plain;charset=utf-8`, in case we
                    // add other mime types consult gtk for normalization.
                    let contents = normilize_to_lf(contents);
                    load_send.send(contents).unwrap();
                }
                // Store text in the primary clipboard
                ThreadRequest::StorePrimary(seat_name, contents) => {
                    event_queue.sync_roundtrip().unwrap();
                    let seat_map = seat_map.lock().unwrap().clone();

                    // Get the requested seat from the seat map
                    if let Some((_, enter_serial, primary_device, _, gtk_primary_device, _)) =
                        seat_map.get(
                            &seat_name.unwrap_or_else(|| last_seat_name.lock().unwrap().clone()),
                        )
                    {
                        if let Some(manager) = &*primary_selection_device_manager.lock().unwrap() {
                            if let Some(primary_device) = &*primary_device.lock().unwrap() {
                                let source = manager.create_source(|proxy| {
                                    proxy.implement_closure(
                                        move |event, _| {
                                            if let zwp_primary_selection_source_v1::Event::Send {
                                                mime_type,
                                                fd,
                                            } = event
                                            {
                                                if mime_type == "text/plain;charset=utf-8" {
                                                    let mut file =
                                                        unsafe { std::fs::File::from_raw_fd(fd) };
                                                    file.write_fmt(format_args!("{}", contents))
                                                        .unwrap();
                                                }
                                            }
                                        },
                                        (),
                                    )
                                });
                                if let Ok(source) = &source {
                                    source.offer("text/plain;charset=utf-8".to_string());
                                }
                                primary_device.set_selection(source.ok().as_ref(), *enter_serial);
                            }
                        } else if let Some(manager) =
                            &*gtk_primary_selection_device_manager.lock().unwrap()
                        {
                            if let Some(gtk_primary_device) = &*gtk_primary_device.lock().unwrap() {
                                let source = manager.create_source(|proxy| {
                                    proxy.implement_closure(
                                        move |event, _| {
                                            if let gtk_primary_selection_source::Event::Send {
                                                mime_type,
                                                fd,
                                            } = event
                                            {
                                                if mime_type == "text/plain;charset=utf-8" {
                                                    let mut file =
                                                        unsafe { std::fs::File::from_raw_fd(fd) };
                                                    file.write_fmt(format_args!("{}", contents))
                                                        .unwrap();
                                                }
                                            }
                                        },
                                        (),
                                    )
                                });
                                if let Ok(source) = &source {
                                    source.offer("text/plain;charset=utf-8".to_string());
                                }
                                gtk_primary_device
                                    .set_selection(source.ok().as_ref(), *enter_serial);
                            }
                        }
                    }
                }
                ThreadRequest::Kill => break,
            }
        }
        // Dispatch the event queue and block for `sleep_amount`
        let pending_events = event_queue.dispatch_pending().unwrap();
        let num_seats = seat_map.lock().unwrap().len();

        // If some app is trying to spam us when there no seats, it's likely that someone is
        // trying to paste from us
        if num_seats == 0 && pending_events != 0 {
            sleep_amount = 0;
        } else if sleep_amount > 0 {
            thread::sleep(Duration::from_millis(sleep_amount));

            if warm_start_amount < 16 {
                warm_start_amount += 1;
                if warm_start_amount == 16 {
                    sleep_amount = 1;
                }
            } else if sleep_amount < 50 {
                // The aim of this different sleep times is to provide a good performance under
                // high load and not waste system resources too much when idle
                sleep_amount = std::cmp::min(2 * sleep_amount, 50);
            }
        } else if sleep_amount == 0 {
            // Reset sleep amount from zero back to one, so sleep sequence could reach 50
            sleep_amount = 1;
            // Reset warm start to accelerate the initial clipboard requests
            warm_start_amount = 0;
        }
    }
}

/// Implement seats that we register
fn implement_seat(
    id: u32,
    version: u32,
    seat_map: Arc<Mutex<SeatMap>>,
    last_seat_name: Arc<Mutex<String>>,
    data_device_manager: &wl_data_device_manager::WlDataDeviceManager,
    reg: &wl_registry::WlRegistry,
    primary_device_manager: Arc<Mutex<Option<PrimarySelectionDeviceMgr>>>,
    gtk_primary_device_manager: Arc<Mutex<Option<GtkPrimarySelectionDeviceManager>>>,
) {
    let seat_name = Arc::new(Mutex::new(String::new()));
    let seat_name_clone = seat_name.clone();

    // Register the seat
    let seat = reg
        .bind::<wl_seat::WlSeat, _>(version, id, move |proxy| {
            proxy.implement_closure(
                move |event, _| {
                    if let wl_seat::Event::Name { name } = event {
                        *seat_name_clone.lock().unwrap() = name
                    }
                },
                (),
            )
        })
        .unwrap();

    // Create a device for the seat
    let device = Arc::new(Mutex::new(DataDevice::init_for_seat(
        data_device_manager,
        &seat,
        |_| {},
    )));

    let primary_offer = Arc::new(Mutex::new(None));
    let primary_offer_clone = primary_offer.clone();
    let gtk_primary_offer = Arc::new(Mutex::new(None));
    let gtk_primary_offer_clone = gtk_primary_offer.clone();
    let seat_map_clone = seat_map.clone();
    let seat_name_clone = seat_name.clone();
    let (primary_device, gtk_primary_device) = if let Some(manager) =
        &*primary_device_manager.lock().unwrap()
    {
        (
            Arc::new(Mutex::new(
                manager
                    .get_device(&seat, |proxy| {
                        let primary_offer_clone = primary_offer_clone.clone();
                        proxy.implement_closure(
                            move |event, _| {
                                if let ZwpPrimarySelectionDeviceEvent::DataOffer { offer } = event {
                                    *primary_offer_clone.lock().unwrap() =
                                        Some(offer.implement_dummy());

                                    let map_contents = seat_map_clone
                                        .lock()
                                        .unwrap()
                                        .get(&seat_name_clone.lock().unwrap().clone())
                                        .cloned();
                                    if let Some(map_contents) = map_contents {
                                        seat_map_clone.lock().unwrap().insert(
                                            seat_name_clone.lock().unwrap().clone(),
                                            (
                                                map_contents.0.clone(),
                                                map_contents.1,
                                                map_contents.2.clone(),
                                                primary_offer_clone.clone(),
                                                Arc::new(Mutex::new(None)),
                                                Arc::new(Mutex::new(None)),
                                            ),
                                        );
                                    }
                                }
                            },
                            (),
                        )
                    })
                    .ok(),
            )),
            Arc::new(Mutex::new(None)),
        )
    } else if let Some(manager) = &*gtk_primary_device_manager.lock().unwrap() {
        (
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(
                manager
                    .get_device(&seat, |proxy| {
                        let gtk_primary_offer_clone = gtk_primary_offer_clone.clone();
                        proxy.implement_closure(
                            move |event, _| {
                                if let GtkPrimarySelectionDeviceEvent::DataOffer { offer } = event {
                                    *gtk_primary_offer_clone.lock().unwrap() =
                                        Some(offer.implement_dummy());

                                    let map_contents = seat_map_clone
                                        .lock()
                                        .unwrap()
                                        .get(&seat_name_clone.lock().unwrap().clone())
                                        .cloned();
                                    if let Some(map_contents) = map_contents {
                                        seat_map_clone.lock().unwrap().insert(
                                            seat_name_clone.lock().unwrap().clone(),
                                            (
                                                map_contents.0.clone(),
                                                map_contents.1,
                                                Arc::new(Mutex::new(None)),
                                                Arc::new(Mutex::new(None)),
                                                map_contents.4.clone(),
                                                gtk_primary_offer_clone.clone(),
                                            ),
                                        );
                                    }
                                }
                            },
                            (),
                        )
                    })
                    .ok(),
            )),
        )
    } else {
        (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)))
    };

    let seat_map_clone = seat_map.clone();
    let device_clone = device.clone();
    let primary_device_clone = primary_device.clone();
    let primary_offer_clone = primary_offer_clone.clone();
    let gtk_primary_device_clone = gtk_primary_device.clone();
    let gtk_primary_offer_clone = gtk_primary_offer_clone.clone();
    let seat_name_clone = seat_name.clone();
    let last_seat_name_clone = last_seat_name.clone();
    map_keyboard_auto(&seat, move |event, _| {
        // Set this seat as the last to send an event
        *last_seat_name_clone.lock().unwrap() = seat_name_clone.lock().unwrap().clone();

        // Get serials from recieved events from the seat keyboard
        match event {
            KbEvent::Enter { serial, .. } => {
                seat_map_clone.lock().unwrap().insert(
                    seat_name_clone.lock().unwrap().clone(),
                    (
                        device_clone.clone(),
                        serial,
                        primary_device_clone.clone(),
                        primary_offer_clone.clone(),
                        gtk_primary_device_clone.clone(),
                        gtk_primary_offer_clone.clone(),
                    ),
                );
            }
            KbEvent::Key { serial, .. } => {
                seat_map_clone.lock().unwrap().insert(
                    seat_name_clone.lock().unwrap().clone(),
                    (
                        device_clone.clone(),
                        serial,
                        primary_device_clone.clone(),
                        primary_offer_clone.clone(),
                        gtk_primary_device_clone.clone(),
                        gtk_primary_offer_clone.clone(),
                    ),
                );
            }
            KbEvent::Leave { .. } => {
                seat_map_clone
                    .lock()
                    .unwrap()
                    .remove(&*seat_name_clone.lock().unwrap());
            }
            _ => {}
        }
    })
    .unwrap();

    seat.get_pointer(|pointer| {
        pointer.implement_closure(
            move |evt, _| {
                // Set this seat as the last to send an event
                *last_seat_name.lock().unwrap() = seat_name.lock().unwrap().clone();

                // Get serials from recieved events from the seat pointer
                match evt {
                    PtrEvent::Enter { serial, .. } => {
                        if let Some(seat) = seat_map
                            .lock()
                            .unwrap()
                            .get_mut(&seat_name.lock().unwrap().clone())
                        {
                            // Update serial if "seat" is already presented
                            seat.1 = serial;
                            return;
                        }

                        seat_map.lock().unwrap().insert(
                            seat_name.lock().unwrap().clone(),
                            (
                                device.clone(),
                                serial,
                                primary_device.clone(),
                                primary_offer.clone(),
                                gtk_primary_device.clone(),
                                gtk_primary_offer.clone(),
                            ),
                        );
                    }
                    PtrEvent::Button { serial, .. } => {
                        if let Some(seat) = seat_map
                            .lock()
                            .unwrap()
                            .get_mut(&seat_name.lock().unwrap().clone())
                        {
                            // Update serial if seat is already presented
                            seat.1 = serial;
                            return;
                        }

                        // This is for consistency with `PtrEvent::Enter`
                        seat_map.lock().unwrap().insert(
                            seat_name.lock().unwrap().clone(),
                            (
                                device.clone(),
                                serial,
                                primary_device.clone(),
                                primary_offer.clone(),
                                gtk_primary_device.clone(),
                                gtk_primary_offer.clone(),
                            ),
                        );
                    }
                    _ => {}
                }
            },
            (),
        )
    })
    .unwrap();
}

// Normalize \r and \r\n into \n.
//
// Gtk does this for text/plain;charset=utf-8, so following them here, otherwise there is
// a chance of getting extra new lines on load, since they're converting \r and \n into
// \r\n on every store.
fn normilize_to_lf(text: String) -> String {
    text.replace("\r\n", "\n").replace("\r", "\n")
}
