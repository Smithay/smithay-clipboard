//! Smithay Wayland Clipboard
//!
//! Provides access to the wayland clipboard with only requirement being a WlDisplay
//! object
//!
//! ```no_run
//!     let (display, mut event_queue) =
//!         Display::connect_to_env().expect("Failed to connect to the wayland server.");
//!     let mut clipboard = smithay_clipboard::WaylandClipboard::new_threaded(
//!         display.get_display_ptr() as *mut std::ffi::c_void,
//!     );
//!     clipboard.store("Test data");
//!     println!(clipboard.load());
//! ```

#![warn(missing_docs)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Deref;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use sctk::data_device::DataDevice;
use sctk::data_device::DataSource;
use sctk::data_device::DataSourceEvent;
use sctk::keyboard::{map_keyboard_auto, Event as KbEvent};
use sctk::reexports::client::protocol::{wl_data_device_manager, wl_registry, wl_seat};
use sctk::reexports::client::{Display, EventQueue, GlobalEvent, GlobalManager};
use sctk::wayland_client::sys::client::wl_display;

type SeatMap = HashMap<String, (Arc<Mutex<DataDevice>>, u32)>;

enum WaylandRequest {
    Store(String, String),
    Load(String),
    Kill,
}

/// Object representing the Wayland clipboard
pub struct WaylandClipboard {
    request_send: mpsc::Sender<WaylandRequest>,
    load_recv: mpsc::Receiver<String>,
}

impl Drop for WaylandClipboard {
    fn drop(&mut self) {
        self.request_send.send(WaylandRequest::Kill).unwrap()
    }
}

impl WaylandClipboard {
    /// Creates a new WaylandClipboard object
    ///
    /// Spawns a new thread to dispatch messages to the wayland server every
    /// 50ms to ensure the server can read stored data
    pub fn new_threaded(display: &Display) -> Self {
        let (request_send, request_recv) = mpsc::channel::<WaylandRequest>();
        let (load_send, load_recv) = mpsc::channel();
        let display = unsafe { display.get_display_ptr().as_mut().unwrap() };

        std::thread::spawn(move || {
            let (display, mut event_queue) = unsafe { Display::from_external_display(display) };
            Self::clipboard_thread(&display, &mut event_queue, request_recv, load_send);
        });

        WaylandClipboard {
            request_send,
            load_recv,
        }
    }

    /// Creates a new WaylandClipboard object from a mutable `wl_display` ptr
    ///
    /// Spawns a new thread to dispatch messages to the wayland server every
    /// 50ms to ensure the server can read stored data
    pub unsafe fn new_threaded_from_external(display_ptr: *mut wl_display) -> Self {
        let (request_send, request_recv) = mpsc::channel::<WaylandRequest>();
        let (load_send, load_recv) = mpsc::channel();
        let display = display_ptr.as_mut().unwrap();
        std::thread::spawn(move || {
            let (display, mut event_queue) = Display::from_external_display(display);
            Self::clipboard_thread(&display, &mut event_queue, request_recv, load_send);
        });

        WaylandClipboard {
            request_send,
            load_recv,
        }
    }

    fn implement_seat(
        id: u32,
        version: u32,
        seat_map: Arc<Mutex<SeatMap>>,
        data_device_manager: &wl_data_device_manager::WlDataDeviceManager,
        reg: &wl_registry::WlRegistry,
    ) {
        let seat_name = Arc::new(Mutex::new(String::new()));
        let seat_name_clone = seat_name.clone();
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
        let device = Arc::new(Mutex::new(DataDevice::init_for_seat(
            data_device_manager,
            &seat,
            |_| {},
        )));
        map_keyboard_auto(&seat, move |event, _| match event {
            KbEvent::Enter { serial, .. } => {
                seat_map
                    .lock()
                    .unwrap()
                    .insert(seat_name.lock().unwrap().clone(), (device.clone(), serial));
            }
            KbEvent::Leave { .. } => {
                seat_map.lock().unwrap().remove(&*seat_name.lock().unwrap());
            }
            _ => {}
        })
        .unwrap();
    }

    fn clipboard_thread(
        display: &Display,
        event_queue: &mut EventQueue,
        request_recv: mpsc::Receiver<WaylandRequest>,
        load_send: mpsc::Sender<String>,
    ) {
        let seat_map = Arc::new(Mutex::new(SeatMap::new()));

        let data_device_manager = Arc::new(Mutex::new(None));
        let unimplemented_seats = Arc::new(Mutex::new(Vec::new()));

        let data_device_manager_clone = data_device_manager.clone();
        let seat_map_clone = seat_map.clone();
        GlobalManager::new_with_cb(&*display, move |event, reg| {
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
                        Self::implement_seat(
                            id,
                            version,
                            seat_map_clone.clone(),
                            data_device_manager,
                            &reg,
                        );
                    } else {
                        unimplemented_seats.lock().unwrap().push((id, version));
                    }
                } else if "wl_data_device_manager" == interface.as_str() {
                    *data_device_manager_clone.lock().unwrap() = Some(
                        reg.bind::<wl_data_device_manager::WlDataDeviceManager, _>(
                            version,
                            id,
                            |proxy| proxy.implement_dummy(),
                        )
                        .unwrap(),
                    );
                    for (id, version) in unimplemented_seats.lock().unwrap().deref() {
                        if let Some(ref data_device_manager) =
                            data_device_manager_clone.lock().unwrap().deref()
                        {
                            Self::implement_seat(
                                *id,
                                *version,
                                seat_map_clone.clone(),
                                data_device_manager,
                                &reg,
                            );
                        }
                    }
                }
            }
        });

        loop {
            if let Ok(request) = request_recv.try_recv() {
                match request {
                    WaylandRequest::Load(seat_name) => {
                        if let Some((device, _)) = seat_map.lock().unwrap().get(&seat_name) {
                            // Load
                            let mut reader = None;
                            device.lock().unwrap().with_selection(|offer| {
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
                            if let Some(mut reader) = reader {
                                let mut contents = String::new();
                                reader.read_to_string(&mut contents).unwrap();
                                load_send.send(contents).unwrap();
                            } else {
                                load_send.send("".to_string()).unwrap();
                            }
                        }
                    }
                    WaylandRequest::Store(seat_name, contents) => {
                        if let Some((device, enter_serial)) =
                            seat_map.lock().unwrap().get(&seat_name)
                        {
                            if let Some(data_device_manager) =
                                data_device_manager.lock().unwrap().deref()
                            {
                                let data_source = DataSource::new(
                                    &data_device_manager,
                                    &["text/plain;charset=utf-8"],
                                    move |source_event| {
                                        if let DataSourceEvent::Send { mut pipe, .. } = source_event
                                        {
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
                    }
                    WaylandRequest::Kill => break,
                }
            }
            event_queue.dispatch_pending().unwrap();
            sleep(Duration::from_millis(50));
        }
    }

    /// Returns text from the wayland clipboard
    ///
    /// Only works when the window connected to the WlDisplay has
    /// keyboard focus
    pub fn load<S: Into<String>>(&mut self, seat_name: S) -> String {
        self.request_send
            .send(WaylandRequest::Load(seat_name.into()))
            .unwrap();
        self.load_recv.recv().unwrap()
    }

    /// Stores text in the wayland clipboard
    ///
    /// Only works when the window connected to the WlDisplay has
    /// keyboard focus
    pub fn store<S: Into<String>>(&mut self, seat_name: S, text: S) {
        self.request_send
            .send(WaylandRequest::Store(seat_name.into(), text.into()))
            .unwrap()
    }
}
