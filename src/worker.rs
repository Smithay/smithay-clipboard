use std::sync::mpsc::Sender;

use sctk::reexports::calloop::channel::Channel;
use sctk::reexports::calloop::{EventLoop, channel};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::Connection;
use sctk::reexports::client::globals::registry_queue_init;

use crate::data::ClipboardData;
use crate::error::{ClipboardError, Result};
use crate::state::{SelectionTarget, State};

/// Spawn a clipboard worker, which dispatches its own `EventQueue` and handles
/// clipboard requests.
pub fn spawn(
    name: String,
    display: Connection,
    rx_chan: Channel<Command>,
    worker_replier: Sender<Result<Reply>>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            worker_impl(display, rx_chan, worker_replier);
        })
        .ok()
}

/// Clipboard worker thread command.
#[derive(Debug)]
pub enum Command {
    /// Store data to clipboard with specified MIME types (same data for all types).
    Store {
        /// The data to store.
        data: Vec<u8>,
        /// The MIME types to advertise.
        mime_types: Vec<String>,
    },
    /// Store multiple formats to clipboard (different data per MIME type).
    StoreMulti {
        /// List of (data, mime_types) tuples.
        formats: Vec<(Vec<u8>, Vec<String>)>,
    },
    /// Store data to primary selection with specified MIME types.
    StorePrimary {
        /// The data to store.
        data: Vec<u8>,
        /// The MIME types to advertise.
        mime_types: Vec<String>,
    },
    /// Store multiple formats to primary selection.
    StorePrimaryMulti {
        /// List of (data, mime_types) tuples.
        formats: Vec<(Vec<u8>, Vec<String>)>,
    },
    /// Load data from clipboard with preferred MIME types.
    Load {
        /// Preferred MIME types in order of preference.
        mime_types: Vec<String>,
    },
    /// Load data from primary selection with preferred MIME types.
    LoadPrimary {
        /// Preferred MIME types in order of preference.
        mime_types: Vec<String>,
    },
    /// Get available MIME types from clipboard.
    GetMimeTypes,
    /// Get available MIME types from primary selection.
    GetPrimaryMimeTypes,
    /// Shutdown the worker.
    Exit,
}

/// Reply from the clipboard worker.
#[derive(Debug)]
pub enum Reply {
    /// Data loaded from clipboard.
    Data(ClipboardData),
    /// List of available MIME types.
    MimeTypes(Vec<String>),
    /// Operation completed successfully (for store operations).
    #[allow(dead_code)]
    Done,
}

/// Handle clipboard requests.
fn worker_impl(connection: Connection, rx_chan: Channel<Command>, reply_tx: Sender<Result<Reply>>) {
    let (globals, event_queue) = match registry_queue_init(&connection) {
        Ok(data) => data,
        Err(_) => return,
    };

    let mut event_loop = EventLoop::<State>::try_new().unwrap();
    let loop_handle = event_loop.handle();

    let mut state = match State::new(&globals, &event_queue.handle(), loop_handle.clone(), reply_tx)
    {
        Some(state) => state,
        None => return,
    };

    loop_handle
        .insert_source(rx_chan, |event, _, state| {
            if let channel::Event::Msg(event) = event {
                match event {
                    Command::StorePrimary { data, mime_types } => {
                        if state.primary_selection_manager_state.is_some() {
                            state.store_selection(
                                SelectionTarget::Primary,
                                vec![(data, mime_types)],
                            );
                        }
                    },
                    Command::StorePrimaryMulti { formats } => {
                        if state.primary_selection_manager_state.is_some() {
                            state.store_selection(SelectionTarget::Primary, formats);
                        }
                    },
                    Command::Store { data, mime_types } => {
                        if state.data_device_manager_state.is_some() {
                            state.store_selection(
                                SelectionTarget::Clipboard,
                                vec![(data, mime_types)],
                            );
                        }
                    },
                    Command::StoreMulti { formats } => {
                        if state.data_device_manager_state.is_some() {
                            state.store_selection(SelectionTarget::Clipboard, formats);
                        }
                    },
                    Command::Load { mime_types } => {
                        if state.data_device_manager_state.is_some() {
                            if let Err(err) =
                                state.load_selection(SelectionTarget::Clipboard, &mime_types)
                            {
                                let _ = state.reply_tx.send(Err(err));
                            }
                        } else {
                            let _ = state.reply_tx.send(Err(ClipboardError::DataDeviceUnsupported));
                        }
                    },
                    Command::LoadPrimary { mime_types } => {
                        if state.primary_selection_manager_state.is_some() {
                            if let Err(err) =
                                state.load_selection(SelectionTarget::Primary, &mime_types)
                            {
                                let _ = state.reply_tx.send(Err(err));
                            }
                        } else {
                            let _ = state
                                .reply_tx
                                .send(Err(ClipboardError::PrimarySelectionUnsupported));
                        }
                    },
                    Command::GetMimeTypes => {
                        if state.data_device_manager_state.is_some() {
                            match state.get_mime_types(SelectionTarget::Clipboard) {
                                Ok(types) => {
                                    let _ = state.reply_tx.send(Ok(Reply::MimeTypes(types)));
                                },
                                Err(err) => {
                                    let _ = state.reply_tx.send(Err(err));
                                },
                            }
                        } else {
                            let _ = state.reply_tx.send(Err(ClipboardError::DataDeviceUnsupported));
                        }
                    },
                    Command::GetPrimaryMimeTypes => {
                        if state.primary_selection_manager_state.is_some() {
                            match state.get_mime_types(SelectionTarget::Primary) {
                                Ok(types) => {
                                    let _ = state.reply_tx.send(Ok(Reply::MimeTypes(types)));
                                },
                                Err(err) => {
                                    let _ = state.reply_tx.send(Err(err));
                                },
                            }
                        } else {
                            let _ = state
                                .reply_tx
                                .send(Err(ClipboardError::PrimarySelectionUnsupported));
                        }
                    },
                    Command::Exit => state.exit = true,
                }
            }
        })
        .unwrap();

    WaylandSource::new(connection, event_queue).insert(loop_handle).unwrap();

    loop {
        if event_loop.dispatch(None, &mut state).is_err() || state.exit {
            break;
        }
    }
}
