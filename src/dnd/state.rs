use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Write};
use std::mem;
use std::os::unix::io::AsRawFd;

use sctk::data_device_manager::data_offer::DragOffer;
use sctk::data_device_manager::data_source::DragSource;
use sctk::data_device_manager::WritePipe;
use sctk::reexports::calloop::PostAction;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_shm::Format;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::Proxy;
use wayland_backend::client::ObjectId;

use crate::mime::{AsMimeTypes, MimeType};
use crate::state::{set_non_blocking, State};

use super::{DndDestinationRectangle, DndEvent, DndRequest, DndSurface, Icon, OfferEvent};

pub(crate) struct DndState<T> {
    pub(crate) sender: Option<Box<dyn crate::dnd::Sender<T>>>,
    destinations: HashMap<ObjectId, (DndSurface<T>, Vec<DndDestinationRectangle>)>,
    pub(crate) dnd_source: Option<DragSource>,
    active_surface: Option<(DndSurface<T>, Option<DndDestinationRectangle>)>,
    source_actions: DndAction,
    selected_action: DndAction,
    selected_mime: Option<MimeType>,
    pub(crate) icon_surface: Option<WlSurface>,
    pub(crate) source_content: Option<Box<dyn AsMimeTypes>>,
    accept_ctr: u32,
}

impl<T> Default for DndState<T> {
    fn default() -> Self {
        Self {
            sender: Default::default(),
            destinations: Default::default(),
            dnd_source: Default::default(),
            active_surface: None,
            source_actions: DndAction::empty(),
            selected_action: DndAction::empty(),
            selected_mime: None,
            source_content: None,
            accept_ctr: 1,
            icon_surface: None,
        }
    }
}

impl<T> DndState<T> {
    pub(crate) fn selected_action(&mut self, a: DndAction) {
        self.selected_action = a;
        if let Some(tx) = self.sender.as_ref() {
            _ = tx.send(DndEvent::Offer(
                self.active_surface.as_ref().and_then(|(_, d)| d.as_ref().map(|d| d.id)),
                OfferEvent::SelectedAction(a),
            ));
        }
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
                            dnd_state.accept_mime_type(self.dnd_state.accept_ctr, None);
                            self.dnd_state.accept_ctr = self.dnd_state.accept_ctr.wrapping_add(1);
                            self.dnd_state.selected_action = DndAction::empty();
                            self.dnd_state.selected_mime = None;
                        }
                    }
                    return (s.clone(), None);
                };
                if !had_dest.is_some_and(|old_id| old_id == dest.id) {
                    if let (Some((action, preferred_action)), Some(mime_type), Some(dnd_state)) =
                        (actions, mime, dnd_state.as_ref())
                    {
                        if let Some((tx, old_id)) = self.dnd_state.sender.as_ref().zip(had_dest) {
                            _ = tx.send(DndEvent::Offer(
                                Some(old_id),
                                super::OfferEvent::LeaveDestination,
                            ));
                        }
                        if let Some(tx) = self.dnd_state.sender.as_ref() {
                            _ = tx.send(DndEvent::Offer(Some(dest.id), OfferEvent::Enter {
                                x,
                                y,
                                surface: s.s.clone(),
                                mime_types: dest.mime_types.clone(),
                            }));

                            _ = tx.send(DndEvent::Offer(
                                Some(dest.id),
                                OfferEvent::SelectedAction(self.dnd_state.selected_action),
                            ));
                        }
                        dnd_state.set_actions(action, preferred_action);
                        self.dnd_state.selected_mime = Some(mime_type.clone());
                        dnd_state.accept_mime_type(
                            self.dnd_state.accept_ctr,
                            Some(mime_type.to_string()),
                        );
                        self.dnd_state.accept_ctr = self.dnd_state.accept_ctr.wrapping_add(1);
                    }
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

        _ = self.load_dnd(mime, false);
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
        let Some(data_device) = self
            .seats
            .iter()
            .find_map(|(_, s)| s.data_device.as_ref().filter(|dev| dev.inner() == wl_data_device))
        else {
            return;
        };
        let drag_offer = data_device.data().drag_offer();
        if drag_offer.is_none() && self.dnd_state.source_content.is_none() {
            // Ignore cancelled internal DnD
            return;
        }
        self.update_active_surface(surface, x, y, drag_offer.as_ref());
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
        let Some(surface) = self.dnd_state.active_surface.clone().map(|(s, _)| s) else {
            return;
        };
        let Some(data_device) = self
            .seats
            .iter()
            .find_map(|(_, s)| s.data_device.as_ref().filter(|dev| dev.inner() == wl_data_device))
        else {
            return;
        };
        let drag_offer = data_device.data().drag_offer();
        if drag_offer.is_none() && self.dnd_state.source_content.is_none() {
            // Ignore cancelled internal DnD
            return;
        }
        self.update_active_surface(&surface.surface, x, y, drag_offer.as_ref());
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
            DndRequest::InitDnd(sender) => self.dnd_state.sender = Some(sender),
            DndRequest::Surface(s, dests) => {
                if dests.is_empty() {
                    self.dnd_state.destinations.remove(&s.surface.id());
                } else {
                    self.dnd_state.destinations.insert(s.surface.id(), (s, dests));
                }
            },
            DndRequest::StartDnd { internal, source, icon, content, actions } => {
                _ = self.start_dnd(internal, source, icon, content, actions);
            },
            DndRequest::SetAction(a) => {
                _ = self.user_selected_action(a);
            },
            DndRequest::DndEnd => {
                if let Some(s) = self.dnd_state.icon_surface.take() {
                    _ = s.destroy();
                }
                self.dnd_state.source_content = None;
                self.dnd_state.dnd_source = None;
                self.pool.remove(&0);
            },
            DndRequest::Peek(mime_type) => {
                if let Err(err) = self.load_dnd(mime_type, true) {
                    _ = self.reply_tx.send(Err(err));
                }
            },
        };
    }

    fn start_dnd(
        &mut self,
        internal: bool,
        source_surface: DndSurface<T>,
        mut icon: Option<Icon<DndSurface<T>>>,
        content: Box<dyn AsMimeTypes + Send>,
        actions: DndAction,
    ) -> std::io::Result<()> {
        let latest = self
            .latest_seat
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Other, "no events received on any seat"))?;
        let seat = self
            .seats
            .get_mut(latest)
            .ok_or_else(|| Error::new(ErrorKind::Other, "active seat lost"))?;
        let serial = seat.latest_serial;

        let data_device = seat
            .data_device
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::Other, "data device missing"))?;

        let (icon_surface, buffer) = if let Some(i) = icon.take() {
            match i {
                Icon::Surface(s) => (Some(s.surface.clone()), None),
                Icon::Buf { data, width, height, transparent } => {
                    let surface = self.compositor_state.create_surface(&self.queue_handle);
                    self.pool.remove(&0);
                    let (_, wl_buffer, buf) = self
                        .pool
                        .create_buffer(
                            width as i32,
                            width as i32 * 4,
                            height as i32,
                            &0,
                            if transparent { Format::Argb8888 } else { Format::Xrgb8888 },
                        )
                        .map_err(|err| Error::new(ErrorKind::Other, err))?;
                    buf.copy_from_slice(&data);

                    (Some(surface), Some((wl_buffer, width, height)))
                },
            }
        } else {
            (None, None)
        };

        if internal {
            DragSource::start_internal_drag(
                data_device,
                &source_surface.surface,
                icon_surface.as_ref(),
                serial,
            )
        } else {
            let mime_types = content.available();
            let source = self
                .data_device_manager_state
                .as_ref()
                .map(|s| {
                    s.create_drag_and_drop_source(
                        &self.queue_handle,
                        mime_types.iter().map(|m| m.as_ref()),
                        actions,
                    )
                })
                .ok_or_else(|| Error::new(ErrorKind::Other, "data device manager missing"))?;
            source.start_drag(data_device, &source_surface.surface, icon_surface.as_ref(), serial);

            self.dnd_state.dnd_source = Some(source);
            self.dnd_state.source_content = Some(content);
            self.dnd_state.source_actions = actions;
        }

        if let (Some((wl_buffer, width, height)), Some(surface)) = (buffer, icon_surface) {
            surface.damage_buffer(0, 0, width as i32, height as i32);
            surface.attach(Some(wl_buffer), 0, 0);
            surface.commit();

            self.dnd_state.icon_surface = Some(surface);
        }

        Ok(())
    }

    fn user_selected_action(&mut self, a: DndAction) -> std::io::Result<()> {
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
        offer.set_actions(a, a);

        if let Some(mime_type) = self.dnd_state.selected_mime.clone() {
            _ = self.load_dnd(mime_type, false);
        }
        Ok(())
    }

    /// Load data for the given target.
    pub fn load_dnd(&mut self, mut mime_type: MimeType, peek: bool) -> std::io::Result<()> {
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

            loop {
                match file.read(&mut reader_buffer) {
                    Ok(0) => {
                        // only finish if not peeking
                        if !peek {
                            offer.finish();
                        }

                        if peek {
                            _ = state
                                .reply_tx
                                .send(Ok((mem::take(&mut content), mem::take(&mut mime_type))));
                        } else if let Some(tx) = state.dnd_state.sender.as_ref() {
                            let _ = tx.send(DndEvent::Offer(cur_id, OfferEvent::Data {
                                data: mem::take(&mut content),
                                mime_type: mem::take(&mut mime_type),
                            }));
                        }
                        break PostAction::Remove;
                    },
                    Ok(n) => content.extend_from_slice(&reader_buffer[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(err) => {
                        if peek {
                            let _ = state.reply_tx.send(Err(err));
                        }
                        break PostAction::Remove;
                    },
                };
            }
        });

        Ok(())
    }

    pub(crate) fn send_dnd_request(&self, write_pipe: WritePipe, mime: String) {
        let Some(content) = self.dnd_state.source_content.as_ref() else {
            return;
        };
        let Some(mime_type) = MimeType::find_allowed(&[mime], &content.available()) else {
            return;
        };

        // Mark FD as non-blocking so we won't block ourselves.
        unsafe {
            if set_non_blocking(write_pipe.as_raw_fd()).is_err() {
                return;
            }
        }

        // Don't access the content on the state directly, since it could change during
        // the send.
        let contents = content.as_bytes(&mime_type);
        let Some(contents) = contents else {
            return;
        };

        let mut written = 0;
        let _ = self.loop_handle.insert_source(write_pipe, move |_, file, _| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.write(&contents[written..]) {
                    Ok(n) if written + n == contents.len() => {
                        written += n;
                        break PostAction::Remove;
                    },
                    Ok(n) => written += n,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(_) => break PostAction::Remove,
                }
            }
        });
    }
}
