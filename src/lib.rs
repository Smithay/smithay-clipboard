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

use std::io::{Read, Write};
use std::os::raw::c_void;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use sctk::data_device::DataDevice;
use sctk::data_device::DataSource;
use sctk::data_device::DataSourceEvent;
use sctk::keyboard::{map_keyboard_auto, Event as KbEvent};
use sctk::reexports::client::Display;
use sctk::wayland_client::sys::client::wl_display;
use sctk::Environment;

enum WaylandRequest {
    Store(String),
    Load,
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
    pub fn new_threaded(wayland_display: *mut c_void) -> Self {
        let (request_send, request_recv) = mpsc::channel::<WaylandRequest>();
        let (load_send, load_recv) = mpsc::channel();

        let wayland_display = unsafe { (wayland_display as *mut wl_display).as_mut().unwrap() };

        std::thread::spawn(move || {
            let (display, mut event_queue) =
                unsafe { Display::from_external_display(wayland_display as *mut wl_display) };
            let env = Environment::from_display(&*display, &mut event_queue).unwrap();

            let seat = env
                .manager
                .instantiate_range(1, 6, |seat| seat.implement_dummy())
                .unwrap();

            let device = DataDevice::init_for_seat(&env.data_device_manager, &seat, |_| {});

            let enter_serial = Arc::new(Mutex::new(None));
            let my_enter_serial = enter_serial.clone();
            let _keyboard = map_keyboard_auto(&seat, move |event, _| {
                if let KbEvent::Enter { serial, .. } = event {
                    *(my_enter_serial.lock().unwrap()) = Some(serial);
                }
            });

            loop {
                if let Ok(request) = request_recv.try_recv() {
                    match request {
                        WaylandRequest::Load => {
                            // Load
                            let mut reader = None;
                            device.with_selection(|offer| {
                                if let Some(offer) = offer {
                                    offer.with_mime_types(|types| {
                                        for t in types {
                                            if t == "text/plain;charset=utf-8" {
                                                reader = Some(
                                                    offer
                                                        .receive("text/plain;charset=utf-8".into())
                                                        .unwrap(),
                                                );
                                            }
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
                        WaylandRequest::Store(contents) => {
                            let data_source = DataSource::new(
                                &env.data_device_manager,
                                &["text/plain;charset=utf-8"],
                                move |source_event| {
                                    if let DataSourceEvent::Send { mut pipe, .. } = source_event {
                                        write!(pipe, "{}", contents).unwrap();
                                    }
                                },
                            );
                            if let Some(enter_serial) = *enter_serial.lock().unwrap() {
                                device.set_selection(&Some(data_source), enter_serial);
                            }
                            event_queue.sync_roundtrip().unwrap();
                        }
                        WaylandRequest::Kill => break,
                    }
                }
                event_queue.dispatch_pending().unwrap();
                sleep(Duration::from_millis(50));
            }
        });

        WaylandClipboard {
            request_send,
            load_recv,
        }
    }

    /// Returns text from the wayland clipboard
    ///
    /// Only works when the window connected to the WlDisplay has
    /// keyboard focus
    pub fn load(&mut self) -> String {
        self.request_send.send(WaylandRequest::Load).unwrap();
        self.load_recv.recv().unwrap()
    }

    /// Stores text in the wayland clipboard
    ///
    /// Only works when the window connected to the WlDisplay has
    /// keyboard focus
    pub fn store<S: Into<String>>(&mut self, text: S) {
        self.request_send
            .send(WaylandRequest::Store(text.into()))
            .unwrap()
    }
}
