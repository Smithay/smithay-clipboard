//! Smithay Clipboard
//!
//! Provides access to the wayland clipboard with only requirement being a WlDisplay
//! object
//!
//! ```no_run
//! let (display, _) =
//! Display::connect_to_env().expect("Failed to connect to the wayland server.");
//! let mut clipboard = smithay_clipboard::WaylandClipboard::new_threaded(&display);
//! clipboard.store(None, "Test data");
//! println!("{}", clipboard.load(None));
//! ```

#![warn(missing_docs)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Deref;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use sctk::data_device::{DataDevice, DataSource, DataSourceEvent};
use sctk::keyboard::{map_keyboard_auto, Event as KbEvent};
use sctk::reexports::client::protocol::wl_pointer::Event as PtrEvent;
use sctk::reexports::client::protocol::{
    wl_data_device_manager, wl_display::WlDisplay, wl_registry, wl_seat,
};
use sctk::reexports::client::{Display, EventQueue, GlobalEvent, GlobalManager, NewProxy};
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
    last_seat_name: Arc<Mutex<String>>,
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
        let display = display.clone();
        let last_seat_name = Arc::new(Mutex::new(String::new()));

        let last_seat_name_clone = last_seat_name.clone();
        std::thread::spawn(move || {
            let mut event_queue = display.create_event_queue();
            let display = (*display)
                .as_ref()
                .make_wrapper(&event_queue.get_token())
                .unwrap();
            Self::clipboard_thread(
                &display,
                &mut event_queue,
                request_recv,
                load_send,
                last_seat_name_clone,
            );
        });

        WaylandClipboard {
            request_send,
            load_recv,
            last_seat_name,
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
        let last_seat_name = Arc::new(Mutex::new(String::new()));

        let last_seat_name_clone = last_seat_name.clone();
        std::thread::spawn(move || {
            let (display, mut event_queue) = Display::from_external_display(display);
            Self::clipboard_thread(
                &display,
                &mut event_queue,
                request_recv,
                load_send,
                last_seat_name_clone,
            );
        });

        WaylandClipboard {
            request_send,
            load_recv,
            last_seat_name,
        }
    }

    fn implement_seat(
        id: u32,
        version: u32,
        seat_map: Arc<Mutex<SeatMap>>,
        last_seat_name: Arc<Mutex<String>>,
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
        let seat_map_clone = seat_map.clone();
        let device_clone = device.clone();
        let seat_name_clone = seat_name.clone();
        let last_seat_name_clone = last_seat_name.clone();
        map_keyboard_auto(&seat, move |event, _| {
            *last_seat_name_clone.lock().unwrap() = seat_name_clone.lock().unwrap().clone();
            match event {
                KbEvent::Enter { serial, .. } => {
                    seat_map_clone.lock().unwrap().insert(
                        seat_name_clone.lock().unwrap().clone(),
                        (device_clone.clone(), serial),
                    );
                }
                KbEvent::Key { serial, .. } => {
                    seat_map_clone.lock().unwrap().insert(
                        seat_name_clone.lock().unwrap().clone(),
                        (device_clone.clone(), serial),
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
                    *last_seat_name.lock().unwrap() = seat_name.lock().unwrap().clone();
                    match evt {
                        PtrEvent::Enter { serial, .. } => {
                            seat_map.lock().unwrap().insert(
                                seat_name.lock().unwrap().clone(),
                                (device.clone(), serial),
                            );
                        }
                        PtrEvent::Button { serial, .. } => {
                            seat_map.lock().unwrap().insert(
                                seat_name.lock().unwrap().clone(),
                                (device.clone(), serial),
                            );
                        }
                        PtrEvent::Leave { .. } => {
                            seat_map.lock().unwrap().remove(&*seat_name.lock().unwrap());
                        }
                        _ => {}
                    }
                },
                (),
            )
        })
        .unwrap();
    }

    fn clipboard_thread(
        display: &WlDisplay,
        event_queue: &mut EventQueue,
        request_recv: mpsc::Receiver<WaylandRequest>,
        load_send: mpsc::Sender<String>,
        last_seat_name: Arc<Mutex<String>>,
    ) {
        let seat_map = Arc::new(Mutex::new(SeatMap::new()));

        let data_device_manager = Arc::new(Mutex::new(None));
        let mut unimplemented_seats = Vec::new();

        let data_device_manager_clone = data_device_manager.clone();
        let seat_map_clone = seat_map.clone();
        let last_seat_name_clone = last_seat_name.clone();
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
                        Self::implement_seat(
                            id,
                            version,
                            seat_map_clone.clone(),
                            last_seat_name_clone.clone(),
                            data_device_manager,
                            &reg,
                        );
                    } else {
                        unimplemented_seats.push((id, version));
                    }
                } else if "wl_data_device_manager" == interface.as_str() {
                    *data_device_manager_clone.lock().unwrap() = Some(
                        reg.bind::<wl_data_device_manager::WlDataDeviceManager, _>(
                            version,
                            id,
                            NewProxy::implement_dummy,
                        )
                        .unwrap(),
                    );
                    for (id, version) in &unimplemented_seats {
                        Self::implement_seat(
                            *id,
                            *version,
                            seat_map_clone.clone(),
                            last_seat_name_clone.clone(),
                            data_device_manager_clone.lock().unwrap().as_ref().unwrap(),
                            &reg,
                        );
                    }
                }
            }
        });
        event_queue.sync_roundtrip().unwrap();

        loop {
            if let Ok(request) = request_recv.try_recv() {
                match request {
                    WaylandRequest::Load(seat_name) => {
                        let seat_map = seat_map.lock().unwrap().clone();
                        if let Some((device, _)) = seat_map.get(&seat_name) {
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
                                load_send.send(String::new()).unwrap();
                            }
                        } else {
                            load_send.send(String::new()).unwrap();
                        }
                    }
                    WaylandRequest::Store(seat_name, contents) => {
                        let seat_map = seat_map.lock().unwrap().clone();
                        if let Some((device, enter_serial)) = seat_map.get(&seat_name) {
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
                    WaylandRequest::Kill => break,
                }
            }
            event_queue.dispatch_pending().unwrap();
            sleep(Duration::from_millis(50));
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
            .send(WaylandRequest::Load(seat_name.unwrap_or_else(|| {
                self.last_seat_name.lock().unwrap().clone()
            })))
            .unwrap();
        self.load_recv.recv().unwrap()
    }

    /// Stores text in the wayland clipboard
    ///
    /// If provided with a seat name that seat must be in
    /// focus to work. Otherwise if no seat name is provided
    /// the name of the seat to last generate a key or pointer event
    /// is used
    pub fn store(&mut self, seat_name: Option<String>, text: String) {
        self.request_send
            .send(WaylandRequest::Store(
                seat_name.unwrap_or_else(|| self.last_seat_name.lock().unwrap().clone()),
                text,
            ))
            .unwrap()
    }
}
