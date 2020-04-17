use std::sync::{Arc, Mutex};

use sctk::reexports::client::{Attached, DispatchData, Main};

use sctk::reexports::protocols::unstable::primary_selection::v1::client::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1;

use sctk::reexports::protocols::unstable::primary_selection::v1::client::zwp_primary_selection_device_manager_v1;
use sctk::reexports::protocols::unstable::primary_selection::v1::client::zwp_primary_selection_device_v1;
use sctk::reexports::protocols::unstable::primary_selection::v1::client::zwp_primary_selection_offer_v1;
use sctk::reexports::protocols::unstable::primary_selection::v1::client::zwp_primary_selection_source_v1;

use sctk::reexports::client::protocol::wl_registry;

use std::cell::RefCell;
use std::rc::Rc;

use sctk::reexports::client::protocol::wl_seat::WlSeat;

use sctk::data_device::ReadPipe;
use sctk::data_device::WritePipe;

use sctk::seat::{SeatHandling, SeatListener};

use sctk::environment::GlobalHandler;

use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

pub struct PrimarySource {
    pub(crate) source: zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
}

pub enum PrimarySourceEvent {
    Send { mime_type: String, pipe: WritePipe },

    Cancelled,
}

struct OfferInner {
    mime_types: Vec<String>,
    serial: u32,
}

pub struct PrimaryOffer {
    pub(crate) offer: zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
    inner: Arc<Mutex<OfferInner>>,
}

impl PrimaryOffer {
    pub(crate) fn new(
        offer: Main<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,
    ) -> PrimaryOffer {
        let inner = Arc::new(Mutex::new(OfferInner {
            mime_types: Vec::new(),
            serial: 0,
        }));

        let inner2 = inner.clone();
        offer.quick_assign(move |_, event, _| {
            use zwp_primary_selection_offer_v1::Event;
            let mut inner = inner2.lock().unwrap();
            match event {
                Event::Offer { mime_type } => {
                    inner.mime_types.push(mime_type);
                }
                _ => unreachable!(),
            }
        });

        PrimaryOffer {
            offer: offer.detach(),
            inner,
        }
    }

    pub fn with_mime_types<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&[String]) -> T,
    {
        let inner = self.inner.lock().unwrap();
        f(&inner.mime_types)
    }

    pub fn receive(&self, mime_type: String) -> Result<ReadPipe, ()> {
        use nix::fcntl::OFlag;
        use nix::unistd::{close, pipe2};
        // create a pipe
        let (readfd, writefd) = pipe2(OFlag::O_CLOEXEC).map_err(|_| ())?;

        self.offer.receive(mime_type, writefd);

        if let Err(err) = close(writefd) {
            eprintln!("Failed to close write pipe: {}", err);
        }

        Ok(unsafe { FromRawFd::from_raw_fd(readfd) })
    }
}

impl Drop for PrimaryOffer {
    fn drop(&mut self) {
        self.offer.destroy();
    }
}

impl PrimarySource {
    pub fn new<F, S, It>(
        mgr: &Attached<ZwpPrimarySelectionDeviceManagerV1>,
        mime_types: It,
        mut callback: F,
    ) -> Self
    where
        F: FnMut(PrimarySourceEvent, DispatchData) + 'static,
        S: Into<String>,
        It: IntoIterator<Item = S>,
    {
        let source = mgr.create_source();
        source.quick_assign(move |source, event, dispatch_data| {
            primary_source_impl(&source, event, dispatch_data, &mut callback);
        });

        for mime in mime_types {
            source.offer(mime.into());
        }

        Self {
            source: source.detach(),
        }
    }
}

fn primary_source_impl<Impl>(
    source: &zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
    event: zwp_primary_selection_source_v1::Event,
    dispatch_data: DispatchData,
    implem: &mut Impl,
) where
    Impl: FnMut(PrimarySourceEvent, DispatchData),
{
    use zwp_primary_selection_source_v1::Event;
    let event = match event {
        Event::Send { mime_type, fd } => PrimarySourceEvent::Send {
            mime_type,
            pipe: unsafe { FromRawFd::from_raw_fd(fd) },
        },
        Event::Cancelled => {
            source.destroy();
            PrimarySourceEvent::Cancelled
        }
        _ => unreachable!(),
    };
    implem(event, dispatch_data);
}

struct PrimaryDeviceInner {
    selection: Option<PrimaryOffer>,
    know_offers: Vec<PrimaryOffer>,
}

impl PrimaryDeviceInner {
    fn new_offer(
        &mut self,
        offer: Main<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,
    ) {
        self.know_offers.push(PrimaryOffer::new(offer));
    }

    fn set_selection(
        &mut self,
        offer: Option<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,
    ) {
        let offer = match offer {
            Some(offer) => offer,
            None => {
                self.selection = None;
                return;
            }
        };

        if let Some(id) = self
            .know_offers
            .iter()
            .position(|o| o.offer.as_ref().equals(&offer.as_ref()))
        {
            self.selection = Some(self.know_offers.swap_remove(id));
        } else {
            panic!("Compositor set an unknown primary offer for selection.")
        }
    }
}

pub struct PrimaryDevice {
    device: zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    inner: Arc<Mutex<PrimaryDeviceInner>>,
}

enum PrimaryDDInner {
    Ready {
        mgr: Attached<ZwpPrimarySelectionDeviceManagerV1>,
        devices: Vec<(WlSeat, PrimaryDevice)>,
    },
    Pending {
        seats: Vec<WlSeat>,
    },
}

impl PrimaryDDInner {
    fn init_psd_manager(
        &mut self,
        mgr: Attached<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,
    ) {
        let seats = if let PrimaryDDInner::Pending { seats } = self {
            std::mem::replace(seats, Vec::new())
        } else {
            eprintln!("Ignoring second zwp_primary_selection_device_manager.");
            return;
        };

        let mut devices = Vec::new();

        for seat in seats {
            let device = PrimaryDevice::init_for_seat(&mgr, &seat);
            devices.push((seat.clone(), device));
        }

        *self = PrimaryDDInner::Ready { mgr, devices }
    }

    fn new_seat(&mut self, seat: &WlSeat) {
        match self {
            PrimaryDDInner::Ready { mgr, devices } => {
                if devices.iter().any(|(s, _)| s == seat) {
                    // The seat already exists, nothing to do
                    return;
                }

                let device = PrimaryDevice::init_for_seat(mgr, seat);
                devices.push((seat.clone(), device));
            }
            PrimaryDDInner::Pending { seats } => {
                seats.push(seat.clone());
            }
        }
    }

    fn remove_seat(&mut self, seat: &WlSeat) {
        match self {
            PrimaryDDInner::Ready { devices, .. } => devices.retain(|(s, _)| s != seat),
            PrimaryDDInner::Pending { seats } => seats.retain(|s| s != seat),
        }
    }

    fn with_primary_device<F: FnOnce(&PrimaryDevice)>(
        &self,
        seat: &WlSeat,
        f: F,
    ) -> Result<(), ()> {
        match self {
            PrimaryDDInner::Pending { .. } => Err(()),
            PrimaryDDInner::Ready { devices, .. } => {
                for (s, device) in devices {
                    if s == seat {
                        f(device);
                        return Ok(());
                    }
                }
                Err(())
            }
        }
    }

    fn get_mgr(
        &self,
    ) -> Option<Attached<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>>
    {
        match self {
            PrimaryDDInner::Ready { mgr, .. } => Some(mgr.clone()),
            PrimaryDDInner::Pending { .. } => None,
        }
    }
}

pub struct PrimaryDataDeviceHandler {
    inner: Rc<RefCell<PrimaryDDInner>>,
    _listener: SeatListener,
}

impl PrimaryDataDeviceHandler {
    pub fn init<S: SeatHandling>(seat_handler: &mut S) -> Self {
        let inner = Rc::new(RefCell::new(PrimaryDDInner::Pending { seats: Vec::new() }));

        let seat_inner = inner.clone();
        let listener = seat_handler.listen(move |seat, seat_data, _| {
            if seat_data.defunct {
                seat_inner.borrow_mut().remove_seat(&seat);
            } else {
                seat_inner.borrow_mut().new_seat(&seat);
            }
        });

        Self {
            inner,
            _listener: listener,
        }
    }
}

impl GlobalHandler<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>
    for PrimaryDataDeviceHandler
{
    fn created(
        &mut self,
        registry: Attached<wl_registry::WlRegistry>,
        id: u32,
        version: u32,
        _: DispatchData,
    ) {
        let version = std::cmp::min(version, 1);
        let pdmgr = registry
            .bind::<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>(
            version, id,
        );
        self.inner.borrow_mut().init_psd_manager((*pdmgr).clone());
    }

    fn get(
        &self,
    ) -> Option<Attached<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>>
    {
        self.inner.borrow().get_mgr()
    }
}

pub trait PrimarySelectionHandling {
    fn with_primary_device<F: FnOnce(&PrimaryDevice)>(&self, seat: &WlSeat, f: F)
        -> Result<(), ()>;
}

impl PrimarySelectionHandling for PrimaryDataDeviceHandler {
    fn with_primary_device<F: FnOnce(&PrimaryDevice)>(
        &self,
        seat: &WlSeat,
        f: F,
    ) -> Result<(), ()> {
        self.inner.borrow().with_primary_device(seat, f)
    }
}

fn primary_device_impl(
    event: zwp_primary_selection_device_v1::Event,
    inner: &mut PrimaryDeviceInner,
) {
    use zwp_primary_selection_device_v1::Event;
    match event {
        Event::DataOffer { offer } => inner.new_offer(offer),
        Event::Selection { id } => inner.set_selection(id),
        _ => unreachable!(),
    }
}

impl PrimaryDevice {
    pub fn init_for_seat(
        manager: &ZwpPrimarySelectionDeviceManagerV1,
        seat: &WlSeat,
    ) -> PrimaryDevice {
        let inner = Arc::new(Mutex::new(PrimaryDeviceInner {
            selection: None,
            know_offers: Vec::new(),
        }));

        let inner2 = inner.clone();

        let device = manager.get_device(seat);
        device.quick_assign(move |_, event, _| {
            let mut inner = inner2.lock().unwrap();
            primary_device_impl(event, &mut *inner);
        });

        PrimaryDevice {
            device: device.detach(),
            inner,
        }
    }

    pub fn set_selection(&self, source: &Option<PrimarySource>, serial: u32) {
        self.device
            .set_selection(source.as_ref().map(|s| &s.source), serial);
    }

    pub fn with_selection<F, T>(&self, f: F) -> T
    where
        F: FnOnce(Option<&PrimaryOffer>) -> T,
    {
        let inner = self.inner.lock().unwrap();
        f(inner.selection.as_ref())
    }
}

impl Drop for PrimaryDevice {
    fn drop(&mut self) {
        self.device.destroy();
    }
}
