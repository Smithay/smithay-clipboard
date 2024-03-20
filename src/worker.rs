use std::borrow::Cow;
use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::sync::mpsc::Sender;

use sctk::reexports::calloop::channel::Channel;
use sctk::reexports::calloop::{channel, EventLoop};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::Connection;

use crate::mime::{AsMimeTypes, MimeType};
use crate::state::{SelectionTarget, State};

/// Spawn a clipboard worker, which dispatches its own `EventQueue` and handles
/// clipboard requests.
pub fn spawn<T: 'static + Send>(
    name: String,
    display: Connection,
    rx_chan: Channel<Command<T>>,
    worker_replier: Sender<Result<(Vec<u8>, MimeType)>>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            worker_impl(display, rx_chan, worker_replier);
        })
        .ok()
}

/// Clipboard worker thread command.
pub enum Command<T> {
    /// Loads data for the first available mime type in the provided list.
    Load(Cow<'static, [MimeType]>, SelectionTarget),
    /// Store Data with the given mime types.
    Store(Box<dyn AsMimeTypes + Send>, SelectionTarget),
    #[cfg(feature = "dnd")]
    /// Init DnD
    InitDnD(Box<dyn crate::dnd::Sender<T> + Send>),
    /// Shutdown the worker.
    Exit,
    /// Phantom data
    Phantom(PhantomData<T>),
}

/// Handle clipboard requests.
fn worker_impl<T: 'static>(
    connection: Connection,
    rx_chan: Channel<Command<T>>,
    reply_tx: Sender<Result<(Vec<u8>, MimeType)>>,
) {
    let (globals, event_queue) = match registry_queue_init(&connection) {
        Ok(data) => data,
        Err(_) => return,
    };

    let mut event_loop = EventLoop::<State<T>>::try_new().unwrap();
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
                    Command::Exit => state.exit = true,
                    Command::Store(data, target) => {
                        state.store_selection(target, data);
                    },
                    Command::Load(mime_types, SelectionTarget::Clipboard)
                        if state.data_device_manager_state.is_some() =>
                    {
                        if let Err(err) =
                            state.load_selection(SelectionTarget::Clipboard, &mime_types)
                        {
                            let _ = state.reply_tx.send(Err(err));
                        }
                    },
                    Command::Load(mime_types, SelectionTarget::Primary)
                        if state.primary_selection_manager_state.is_some() =>
                    {
                        if let Err(err) =
                            state.load_selection(SelectionTarget::Primary, &mime_types)
                        {
                            let _ = state.reply_tx.send(Err(err));
                        }
                    },
                    Command::Load(..) => {
                        let _ = state.reply_tx.send(Err(Error::new(
                            ErrorKind::Other,
                            "requested selection is not supported",
                        )));
                    },
                    #[cfg(feature = "dnd")]
                    Command::InitDnD(sender) => {
                        state.sender = Some(sender);
                    },
                    Command::Phantom(_) => unreachable!(),
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
