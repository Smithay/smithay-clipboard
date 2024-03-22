use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read};
use std::mem;
use std::os::unix::io::AsRawFd;
use std::rc::Rc;

use sctk::data_device_manager::data_offer::DragOffer;
use sctk::data_device_manager::data_source::DragSource;
use sctk::reexports::calloop::PostAction;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::Proxy;
use wayland_backend::client::ObjectId;

use crate::mime::{AsMimeTypes, MimeType};
use crate::state::{set_non_blocking, State};
use crate::text::Text;

use super::{DndDestinationRectangle, DndEvent, DndRequest, DndSurface, OfferEvent};

pub(crate) struct DndState<T> {
    pub(crate) sender: Option<Box<dyn crate::dnd::Sender<T>>>,
    destinations: HashMap<ObjectId, (DndSurface<T>, Vec<DndDestinationRectangle>)>,
    dnd_sources: Option<DragSource>,
    active_surface: Option<(DndSurface<T>, Option<DndDestinationRectangle>)>,
    source_actions: DndAction,
    selected_action: DndAction,
    selected_mime: Option<MimeType>,
    pub(crate) source_content: Box<dyn AsMimeTypes>,
    pub(crate) source_mime_types: Rc<Cow<'static, [MimeType]>>,
}

impl<T> Default for DndState<T> {
    fn default() -> Self {
        Self {
            sender: Default::default(),
            destinations: Default::default(),
            dnd_sources: Default::default(),
            active_surface: None,
            source_actions: DndAction::empty(),
            selected_action: DndAction::empty(),
            selected_mime: None,
            source_content: Box::new(Text(String::new())),
            source_mime_types: Rc::new(Cow::Owned(Vec::new())),
        }
    }
}

impl<T> DndState<T> {
    pub(crate) fn selected_action(&mut self, a: DndAction) {
        self.selected_action = a;
    }
}

impl<T> State<T>
where
    T: Clone + 'static,
{
    pub fn update_active_surface(
        &mut self,
        surface: &WlSurface,
        x: f64,
        y: f64,
        dnd_state: Option<&DragOffer>,
    ) {
        let had_dest = self
            .dnd_state
            .active_surface
            .as_ref()
            .map(|(_, d)| d.as_ref().map(|d| d.id))
            .unwrap_or_default();
        self.dnd_state.active_surface =
            self.dnd_state.destinations.get(&surface.id()).map(|(s, dests)| {
                let Some((dest, mime, actions)) = dests.iter().find_map(|r| {
                    let actions = dnd_state.as_ref().map(|s| {
                        (
                            s.source_actions.intersection(r.actions),
                            s.source_actions.intersection(r.preferred),
                        )
                    });
                    let mime = dnd_state.as_ref().and_then(|dnd_state| {
                        r.mime_types.iter().find(|m| {
                            dnd_state.with_mime_types(|mimes| mimes.iter().any(|a| a == m.as_ref()))
                        })
                    });

                    (r.rectangle.contains(x, y)
                        && (r.mime_types.is_empty() || mime.is_some())
                        && (r.actions.is_all()
                            || dnd_state
                                .as_ref()
                                .map(|dnd_state| dnd_state.source_actions.intersects(r.actions))
                                .unwrap_or(true)))
                    .then(|| (r.clone(), mime, actions))
                }) else {
                    if let Some(old_id) = had_dest {
                        if let Some(dnd_state) = dnd_state.as_ref() {
                            if let Some(tx) = self.dnd_state.sender.as_ref() {
                                _ = tx.send(DndEvent::Offer(
                                    Some(old_id),
                                    super::OfferEvent::LeaveDestination,
                                ));
                            }
                            dnd_state.set_actions(DndAction::empty(), DndAction::empty());
                            dnd_state.accept_mime_type(dnd_state.serial, None);
                            self.dnd_state.selected_action = DndAction::empty();
                            self.dnd_state.selected_mime = None;
                        }
                    }
                    return (s.clone(), None);
                };
                if let (Some((action, preferred_action)), Some(mime_type), Some(dnd_state)) =
                    (actions, mime, dnd_state.as_ref())
                {
                    dnd_state.set_actions(action, preferred_action);
                    self.dnd_state.selected_mime = Some(mime_type.clone());
                    dnd_state.accept_mime_type(dnd_state.serial, Some(mime_type.to_string()))
                }
                (s.clone(), Some(dest))
            });
    }

    fn cur_id(&self) -> Option<u128> {
        self.dnd_state.active_surface.as_ref().and_then(|(_, rect)| rect.as_ref().map(|r| r.id))
    }

    pub(crate) fn offer_drop(&mut self, wl_data_device: &WlDataDevice) {
        let Some(tx) = self.dnd_state.sender.as_ref() else {
            return;
        };
        let id = self.cur_id();
        _ = tx.send(DndEvent::Offer(id, super::OfferEvent::Drop));

        let Some(data_device) = self
            .seats
            .iter()
            .find_map(|(_, s)| s.data_device.as_ref().filter(|dev| dev.inner() == wl_data_device))
        else {
            return;
        };
        let Some(dnd_state) = data_device.data().drag_offer() else {
            return;
        };

        if self.dnd_state.selected_action == DndAction::Ask {
            _ = tx.send(DndEvent::Offer(
                id,
                super::OfferEvent::SelectedAction(self.dnd_state.selected_action),
            ));
            return;
        } else if self.dnd_state.selected_action.is_empty() {
            return;
        }
        let Some(mime) = self.dnd_state.selected_mime.take() else {
            dnd_state.accept_mime_type(dnd_state.serial, None);
            return;
        };

        dnd_state.set_actions(self.dnd_state.selected_action, self.dnd_state.selected_action);
        dnd_state.accept_mime_type(dnd_state.serial, Some(mime.to_string()));

        _ = self.load_dnd(mime);
    }

    pub(crate) fn offer_enter(
        &mut self,
        x: f64,
        y: f64,
        surface: &WlSurface,
        wl_data_device: &WlDataDevice,
    ) {
        if self.dnd_state.sender.is_none() {
            return;
        }
        dbg!(&self.dnd_state.destinations);
        let Some(data_device) = self
            .seats
            .iter()
            .find_map(|(_, s)| s.data_device.as_ref().filter(|dev| dev.inner() == wl_data_device))
        else {
            return;
        };
        let dnd_state = data_device.data().drag_offer();
        self.update_active_surface(surface, x, y, dnd_state.as_ref());
        let Some((surface, id)) = self
            .dnd_state
            .active_surface
            .as_ref()
            .map(|(s, d)| (s.clone(), d.as_ref().map(|d| d.id)))
        else {
            return;
        };
        let Some(tx) = self.dnd_state.sender.as_ref() else {
            return;
        };
        // TODO accept mime / action
        _ = tx.send(DndEvent::Offer(id, super::OfferEvent::Enter {
            x,
            y,
            surface: surface.s,
            mime_types: Vec::new(),
        }));
    }

    pub(crate) fn offer_motion(&mut self, x: f64, y: f64, wl_data_device: &WlDataDevice) {
        let Some((surface, dest)) = self
            .dnd_state
            .active_surface
            .clone()
            .map(|(s, dest)| (s, dest.filter(|d| d.rectangle.contains(x, y))))
        else {
            return;
        };
        let Some(data_device) = self
            .seats
            .iter()
            .find_map(|(_, s)| s.data_device.as_ref().filter(|dev| dev.inner() == wl_data_device))
        else {
            return;
        };
        let dnd_state = data_device.data().drag_offer();
        if dest.is_none() {
            self.update_active_surface(&surface.surface, x, y, dnd_state.as_ref());
        }
        let id = self.cur_id();
        if let Some(tx) = self.dnd_state.sender.as_ref() {
            _ = tx.send(DndEvent::Offer(id, super::OfferEvent::Motion { x, y }));
        }
    }

    pub(crate) fn offer_leave(&mut self) {
        if let Some(tx) = self.dnd_state.sender.as_ref() {
            self.dnd_state.active_surface = None;
            self.dnd_state.selected_action = DndAction::empty();
            self.dnd_state.selected_mime = None;
            _ = tx.send(DndEvent::Offer(None, super::OfferEvent::Leave))
        }
    }

    pub(crate) fn handle_dnd_request(&mut self, r: DndRequest<T>) {
        match r {
            DndRequest::InitDnD(sender) => self.dnd_state.sender = Some(sender),
            DndRequest::Surface(s, dests) => {
                self.dnd_state.destinations.insert(s.surface.id(), (s, dests));
            },
            DndRequest::StartDnd { source, icon, content } => {},
            DndRequest::SetAction(_) => {
                todo!()
            },
        };
    }

    /// Load data for the given target.
    pub fn load_dnd(&mut self, mut mime_type: MimeType) -> std::io::Result<()> {
        let cur_id = self.cur_id();
        let latest = self
            .latest_seat
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Other, "no events received on any seat"))?;
        let seat = self
            .seats
            .get_mut(latest)
            .ok_or_else(|| Error::new(ErrorKind::Other, "active seat lost"))?;

        let offer = seat
            .data_device
            .as_ref()
            .and_then(|d| d.data().drag_offer())
            .ok_or_else(|| Error::new(ErrorKind::Other, "offer does not exist."))?;

        let read_pipe = { offer.receive(mime_type.to_string())? };

        // Mark FD as non-blocking so we won't block ourselves.
        unsafe {
            set_non_blocking(read_pipe.as_raw_fd())?;
        }

        let mut reader_buffer = [0; 4096];
        let mut content = Vec::new();
        let _ = self.loop_handle.insert_source(read_pipe, move |_, file, state| {
            let file = unsafe { file.get_mut() };
            let Some(tx) = state.dnd_state.sender.as_ref() else {
                return PostAction::Remove;
            };
            loop {
                match file.read(&mut reader_buffer) {
                    Ok(0) => {
                        offer.finish();
                        let _ = tx.send(DndEvent::Offer(cur_id, OfferEvent::Data {
                            data: mem::take(&mut content),
                            mime_type: mem::take(&mut mime_type).to_string(),
                        }));
                        break PostAction::Remove;
                    },
                    Ok(n) => content.extend_from_slice(&reader_buffer[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(_) => {
                        // let _ = state.dnd_state.sender.unwrap().send(Err(err));
                        break PostAction::Remove;
                    },
                };
            }
        });

        Ok(())
    }
}
