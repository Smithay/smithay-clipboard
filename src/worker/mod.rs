use std::io::prelude::*;
use std::io::Result;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use sctk::reexports::client::protocol::wl_data_device_manager::WlDataDeviceManager;
use sctk::reexports::client::Display;

use sctk::data_device::DataSourceEvent;
use sctk::primary_selection::PrimarySelectionSourceEvent;

use sctk::environment::Environment;
use sctk::seat;

use crate::env::SmithayClipboard;
use crate::mime::{self, MimeType};

mod dispatch_data;
mod handlers;
mod seat_data;
mod sleep_amount_tracker;

use dispatch_data::ClipboardDispatchData;
use seat_data::SeatData;
use sleep_amount_tracker::SleepAmountTracker;

/// Max time clipboard thread can sleep.
const MAX_TIME_TO_SLEEP: u8 = 50;

/// Max warm wakeups.
const MAX_WARM_WAKEUPS: u8 = 16;

/// Spawn a clipboard worker, which dispatches it's own `EventQueue` each 50ms and handles
/// clipboard requests.
pub fn spawn(
    name: String,
    display: Display,
    worker_receiver: Receiver<Command>,
    worker_replier: Sender<Result<String>>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            worker_impl(display, worker_receiver, worker_replier);
        })
        .ok()
}

/// Clipboard worker thread command.
#[derive(Eq, PartialEq)]
pub enum Command {
    /// Store data to a clipboard.
    Store(String),
    /// Store data to a primary selection.
    StorePrimary(String),
    /// Load data from a clipboard.
    Load,
    /// Load primary selection.
    LoadPrimary,
    /// Shutdown the worker.
    Exit,
}

/// Handle clipboard requests.
fn worker_impl(display: Display, request_rx: Receiver<Command>, reply_tx: Sender<Result<String>>) {
    let mut queue = display.create_event_queue();
    let display_proxy = display.attach(queue.token());

    let env = Environment::init(&display_proxy, SmithayClipboard::new());
    let req = queue.sync_roundtrip(&mut (), |_, _, _| unreachable!());
    let req = req.and_then(|_| queue.sync_roundtrip(&mut (), |_, _, _| unreachable!()));

    // We shouldn't crash the application if we've failed to dispatch.
    if req.is_err() {
        return;
    }

    // Get data device manager.
    let data_device_manager = env.get_global::<WlDataDeviceManager>();

    // Get primary selection device manager.
    let primary_selection_manager = env.get_primary_selection_manager();

    // Both clipboards are not available, spin the loop and reply to a clipboard master.
    if data_device_manager.is_none() && primary_selection_manager.is_none() {
        loop {
            if let Ok(event) = request_rx.recv() {
                match event {
                    Command::Exit => {
                        return;
                    }
                    _ => {
                        // Reply with error
                        handlers::reply_error(&reply_tx, "Clipboard are missing.");
                    }
                }
            }
        }
    }

    // Track seats.
    let mut seats = Vec::<SeatData>::new();

    for seat in env.get_all_seats() {
        let seat_data = match seat::clone_seat_data(&seat) {
            Some(seat_data) => {
                // Handle defunct seats early on.
                if seat_data.defunct {
                    seats.push(SeatData::new(seat.detach(), None, None));
                    continue;
                }

                seat_data
            }
            _ => continue,
        };

        let keyboard = if seat_data.has_keyboard {
            let keyboard = seat.get_keyboard();
            let seat_clone = seat.clone();

            keyboard.quick_assign(move |_keyboard, event, dispatch_data| {
                handlers::keyboard_handler(seat_clone.detach(), event, dispatch_data);
            });

            Some(keyboard.detach())
        } else {
            None
        };

        let pointer = if seat_data.has_pointer {
            let pointer = seat.get_pointer();
            let seat_clone = seat.clone();

            pointer.quick_assign(move |_pointer, event, dispatch_data| {
                handlers::pointer_handler(seat_clone.detach(), event, dispatch_data);
            });

            Some(pointer.detach())
        } else {
            None
        };

        // Track the seat.
        seats.push(SeatData::new(seat.detach(), keyboard, pointer));
    }

    // Listen for seats.
    let _listener = env.listen_for_seats(move |seat, seat_data, _| {
        let detached_seat = seat.detach();
        let pos = seats.iter().position(|st| st.seat == detached_seat);
        let index = pos.unwrap_or_else(|| {
            seats.push(SeatData::new(detached_seat, None, None));
            seats.len() - 1
        });

        let seat_resources = &mut seats[index];

        if seat_data.has_keyboard && !seat_data.defunct {
            if seat_resources.keyboard.is_none() {
                let keyboard = seat.get_keyboard();
                let seat_clone = seat.clone();

                keyboard.quick_assign(move |_keyboard, event, dispatch_data| {
                    handlers::keyboard_handler(seat_clone.detach(), event, dispatch_data);
                });

                seat_resources.keyboard = Some(keyboard.detach());
            }
        } else {
            // Clean up.
            if let Some(keyboard) = seat_resources.keyboard.take() {
                if keyboard.as_ref().version() >= 3 {
                    keyboard.release();
                }
            }
        }

        if seat_data.has_pointer && !seat_data.defunct {
            if seat_resources.pointer.is_none() {
                let pointer = seat.get_pointer();

                pointer.quick_assign(move |_pointer, event, dispatch_data| {
                    handlers::pointer_handler(seat.detach(), event, dispatch_data);
                });

                seat_resources.pointer = Some(pointer.detach());
            }
        } else if let Some(pointer) = seat_resources.pointer.take() {
            // Clean up.
            if pointer.as_ref().version() >= 3 {
                pointer.release();
            }
        }
    });

    // Flush the display.
    let _ = queue.display().flush();

    let mut dispatch_data = ClipboardDispatchData::new();

    // Setup sleep amount tracker.
    let mut sa_tracker = SleepAmountTracker::new(MAX_TIME_TO_SLEEP, MAX_WARM_WAKEUPS);

    loop {
        // Try to get event from the user.
        if let Ok(request) = request_rx.try_recv() {
            // Break early on to handle shutdown gracefully, otherwise we can crash on
            // `sync_roundtrip`, if client closed connection to a server before releasing the
            // clipboard.
            if request == Command::Exit {
                break;
            }
            // Reset the time we're sleeping.
            sa_tracker.reset_sleep();

            if queue.sync_roundtrip(&mut dispatch_data, |_, _, _| unimplemented!()).is_err() {
                if request == Command::LoadPrimary || request == Command::Load {
                    handlers::reply_error(&reply_tx, "primary clipboard is not available.");
                    break;
                }
            }

            // Get latest observed seat and serial.
            let (seat, serial) = match dispatch_data.last_seat() {
                Some(data) => data,
                None => {
                    handlers::reply_error(&reply_tx, "no focus on a seat.");
                    continue;
                }
            };

            let serial = *serial;

            // Handle requests.
            match request {
                Command::Load => {
                    if data_device_manager.is_some() {
                        handle_load!(env, with_data_device, seat, queue, reply_tx);
                    } else {
                        handlers::reply_error(&reply_tx, "clipboard is not available.");
                    }
                }
                Command::Store(contents) => {
                    if data_device_manager.is_some() {
                        handle_store!(
                            env,
                            new_data_source,
                            with_data_device,
                            DataSourceEvent,
                            seat,
                            serial,
                            queue,
                            contents
                        );
                    }
                }
                Command::LoadPrimary => {
                    if primary_selection_manager.is_some() {
                        handle_load!(env, with_primary_selection, seat, queue, reply_tx);
                    } else {
                        handlers::reply_error(&reply_tx, "primary clipboard is not available.");
                    }
                }
                Command::StorePrimary(contents) => {
                    if primary_selection_manager.is_some() {
                        handle_store!(
                            env,
                            new_primary_selection_source,
                            with_primary_selection,
                            PrimarySelectionSourceEvent,
                            seat,
                            serial,
                            queue,
                            contents
                        );
                    }
                }
                _ => unreachable!(),
            }
        }

        let pending_events = match queue.dispatch_pending(&mut dispatch_data, |_, _, _| {}) {
            Ok(pending_events) => pending_events,
            Err(_) => break,
        };

        // If some application is trying to spam us when there're no seats, it's likely that
        // someone is trying to paste from us.
        if dispatch_data.last_seat().is_none() && pending_events != 0 {
            sa_tracker.reset_sleep();
        } else {
            // Time for thread to sleep.
            let tts = sa_tracker.sleep_amount();
            if tts > 0 {
                std::thread::sleep(Duration::from_millis(tts as _));
            }

            sa_tracker.increase_sleep();
        }
    }
}
