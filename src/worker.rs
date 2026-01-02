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
    /// DnD request (only with "dnd" feature).
    #[cfg(feature = "dnd")]
    Dnd(DndCommand),
    /// Shutdown the worker.
    Exit,
}

/// DnD-specific commands (only available with the "dnd" feature).
#[cfg(feature = "dnd")]
pub enum DndCommand {
    /// Initialize DnD with the event sender.
    InitDnd(Box<dyn crate::dnd::Sender<sctk::reexports::client::protocol::wl_surface::WlSurface> + Send>),
    /// Register a surface for DnD destination.
    RegisterDestination {
        /// The surface to register.
        surface: sctk::reexports::client::protocol::wl_surface::WlSurface,
        /// The destination rectangles.
        rectangles: Vec<crate::dnd::DndDestinationRectangle>,
    },
    /// Start a DnD operation.
    StartDnd {
        /// The source surface.
        source: sctk::reexports::client::protocol::wl_surface::WlSurface,
        /// The data to drag.
        data: crate::dnd::DndData,
        /// Allowed actions.
        actions: sctk::reexports::client::protocol::wl_data_device_manager::DndAction,
        /// Optional icon surface.
        icon: Option<sctk::reexports::client::protocol::wl_surface::WlSurface>,
    },
    /// End the current DnD operation.
    EndDnd,
    /// Set the DnD action.
    SetAction(sctk::reexports::client::protocol::wl_data_device_manager::DndAction),
    /// Peek at the DnD offer data.
    Peek(String),
    /// Finish the DnD operation (accept the drop).
    Finish,
}

#[cfg(feature = "dnd")]
impl std::fmt::Debug for DndCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InitDnd(_) => f.debug_tuple("InitDnd").finish(),
            Self::RegisterDestination { surface, rectangles } => {
                f.debug_struct("RegisterDestination")
                    .field("surface", surface)
                    .field("rectangles", rectangles)
                    .finish()
            }
            Self::StartDnd { source, data, actions, icon } => {
                f.debug_struct("StartDnd")
                    .field("source", source)
                    .field("data", data)
                    .field("actions", actions)
                    .field("icon", icon)
                    .finish()
            }
            Self::EndDnd => write!(f, "EndDnd"),
            Self::SetAction(action) => f.debug_tuple("SetAction").field(action).finish(),
            Self::Peek(mime) => f.debug_tuple("Peek").field(mime).finish(),
            Self::Finish => write!(f, "Finish"),
        }
    }
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
                    #[cfg(feature = "dnd")]
                    Command::Dnd(dnd_cmd) => {
                        match dnd_cmd {
                            DndCommand::InitDnd(sender) => {
                                state.init_dnd(sender);
                            },
                            DndCommand::RegisterDestination { surface, rectangles } => {
                                state.register_dnd_destination(surface, rectangles);
                            },
                            DndCommand::StartDnd { source, data, actions, icon } => {
                                let _ = state.start_dnd(&source, data, actions, icon.as_ref());
                            },
                            DndCommand::EndDnd => {
                                state.end_dnd();
                            },
                            DndCommand::SetAction(action) => {
                                state.set_dnd_action(action);
                            },
                            DndCommand::Peek(mime_type) => {
                                if let Err(err) = state.peek_dnd_offer(&mime_type) {
                                    let _ = state.reply_tx.send(Err(err));
                                }
                            },
                            DndCommand::Finish => {
                                state.finish_dnd();
                            },
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
