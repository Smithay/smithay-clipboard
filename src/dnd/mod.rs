use std::ffi::c_void;
use std::fmt::Debug;
use std::sync::mpsc::SendError;

use sctk::reexports::calloop;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy};
use wayland_backend::client::{InvalidId, ObjectId};

use crate::Clipboard;

pub struct DndSurface<T> {
    pub surface: WlSurface,
    pub s: T,
}

impl<T> Debug for DndSurface<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface").field("surface", &self.surface).finish()
    }
}

impl<T: RawSurface> DndSurface<T> {
    fn new(mut s: T, conn: &Connection) -> Result<Self, InvalidId> {
        let ptr = unsafe { s.get_ptr() };
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), ptr.cast())? };
        let surface = WlSurface::from_id(conn, id)?;
        Ok(Self { s, surface })
    }
}

impl RawSurface for WlSurface {
    unsafe fn get_ptr(&mut self) -> *mut c_void {
        self.id().as_ptr().cast()
    }
}

pub trait RawSurface {
    /// must return a valid `*mut wl_surface` pointer, and it must remain
    /// valid for as long as this object is alive.
    unsafe fn get_ptr(&mut self) -> *mut c_void;
}

pub trait Sender<T> {
    /// Send an event in the channel
    fn send(&self, t: DndEvent<T>) -> Result<(), SendError<DndEvent<T>>>;
}

#[derive(Debug)]
pub enum DndEvent<T> {
    Test(T),
}

impl<T> Sender<T> for calloop::channel::Sender<DndEvent<T>> {
    fn send(&self, t: DndEvent<T>) -> Result<(), SendError<DndEvent<T>>> {
        self.send(t)
    }
}

impl<T> Sender<T> for calloop::channel::SyncSender<DndEvent<T>> {
    fn send(&self, t: DndEvent<T>) -> Result<(), SendError<DndEvent<T>>> {
        self.send(t)
    }
}

impl<T> Clipboard<T> {
    /// Set up DnD operations for the Clipboard
    pub fn init_dnd(
        &self,
        tx: Box<dyn Sender<T> + Send>,
    ) -> Result<(), SendError<crate::worker::Command<T>>> {
        self.request_sender.send(crate::worker::Command::InitDnD(tx))
    }

    /// Start a DnD operation on the given surface with some data
    pub fn start_dnd<D: RawSurface>(&self, s: D) {
        let s = DndSurface::new(s, &self.connection).unwrap();
        dbg!(&s.surface);
    }

    /// End the current DnD operation, if there is one
    pub fn end_dnd() {}
}
