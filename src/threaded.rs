use std::collections::HashMap;
use std::io::{Read, Write};
use std::ops::Deref;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use sctk::data_device::{DataDevice, DataSource, DataSourceEvent};
use sctk::keyboard::{map_keyboard_auto, Event as KbEvent};
use sctk::reexports::client::protocol::{
    wl_data_device_manager, wl_display::WlDisplay, wl_pointer::Event as PtrEvent, wl_registry,
    wl_seat,
};
use sctk::reexports::client::{Display, EventQueue, GlobalEvent, GlobalManager, NewProxy};
use sctk::wayland_client::sys::client::wl_display;

/// Used to store registered seats and their last event serial
type SeatMap = HashMap<String, (Arc<Mutex<DataDevice>>, u32)>;

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
    pub unsafe fn new_threaded_from_external(display_ptr: *mut wl_display) -> Self {
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
    pub fn store(&mut self, seat_name: Option<String>, text: String) {
        self.request_send
            .send(ThreadRequest::Store(seat_name, text))
            .unwrap()
    }
}

/// Requests sent to the clipboard thread
enum ThreadRequest {
    /// Store text in a specific seats clipboard
    Store(Option<String>, String),
    /// Load text from a specific seats clipboard
    Load(Option<String>),
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

    // Store the name of the seat that last sends an event for use as the default seat
    let last_seat_name = Arc::new(Mutex::new(String::new()));

    let data_device_manager_clone = data_device_manager.clone();
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
                    );
                }
            }
        }
    });
    event_queue.sync_roundtrip().unwrap();

    // Thread loop to handle requests and dispatch the event queue
    loop {
        if let Ok(request) = request_recv.try_recv() {
            match request {
                // Load text from clipboard
                ThreadRequest::Load(seat_name) => {
                    event_queue.sync_roundtrip().unwrap();
                    let seat_map = seat_map.lock().unwrap().clone();

                    // Get the requested seat from the seat map
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
                    load_send.send(contents).unwrap();
                }
                // Store text in the clipboard
                ThreadRequest::Store(seat_name, contents) => {
                    event_queue.sync_roundtrip().unwrap();
                    let seat_map = seat_map.lock().unwrap().clone();

                    // Get the requested seat from the seat map
                    if let Some((device, enter_serial)) = seat_map
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
                ThreadRequest::Kill => break,
            }
        }
        // Dispatch the event queue and block for 50 milliseconds
        event_queue.dispatch_pending().unwrap();
        sleep(Duration::from_millis(50));
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

    let seat_map_clone = seat_map.clone();
    let device_clone = device.clone();
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
                // Set this seat as the last to send an event
                *last_seat_name.lock().unwrap() = seat_name.lock().unwrap().clone();

                // Get serials from recieved events from the seat pointer
                match evt {
                    PtrEvent::Enter { serial, .. } => {
                        seat_map
                            .lock()
                            .unwrap()
                            .insert(seat_name.lock().unwrap().clone(), (device.clone(), serial));
                    }
                    PtrEvent::Button { serial, .. } => {
                        seat_map
                            .lock()
                            .unwrap()
                            .insert(seat_name.lock().unwrap().clone(), (device.clone(), serial));
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
