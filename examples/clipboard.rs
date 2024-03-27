// The example just demonstrates how to integrate the smithay-clipboard into the
// application. For more details on what is going on, consult the
// `smithay-client-toolkit` examples.

use std::borrow::Cow;
use std::ffi::c_void;
use std::str::{FromStr, Utf8Error};

use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::{self, EventLoop, LoopHandle};
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use sctk::seat::pointer::{PointerEventKind, PointerHandler, BTN_LEFT, BTN_RIGHT};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shell::xdg::XdgShell;
use sctk::shell::WaylandSurface;
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use smithay_clipboard::dnd::{DndDestinationRectangle, Icon, OfferEvent, Rectangle, SourceEvent};
use smithay_clipboard::mime::{AllowedMimeTypes, AsMimeTypes, MimeType, ALLOWED_TEXT_MIME_TYPES};
use smithay_clipboard::{Clipboard, SimpleClipboard};
use thiserror::Error;
use url::Url;

const MIN_DIM_SIZE: usize = 256;

#[allow(dead_code)]
// Example usage if RawSurface weren't already implemented
struct MySurface(WlSurface);

// Example usage if RawSurface weren't already implemented
impl smithay_clipboard::dnd::RawSurface for MySurface {
    unsafe fn get_ptr(&mut self) -> *mut c_void {
        self.0.id().as_ptr().cast()
    }
}

fn main() {
    let connection = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&connection).unwrap();
    let queue_handle = event_queue.handle();
    let mut event_loop: EventLoop<SimpleWindow> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    WaylandSource::new(connection.clone(), event_queue).insert(loop_handle).unwrap();

    let compositor =
        CompositorState::bind(&globals, &queue_handle).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &queue_handle).expect("xdg shell is not available");

    let shm = Shm::bind(&globals, &queue_handle).expect("wl shm is not available.");
    let surface = compositor.create_surface(&queue_handle);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &queue_handle);

    window.set_title(String::from("smithay-clipboard example. Press C/c/P/p to copy/paste"));
    window.set_min_size(Some((MIN_DIM_SIZE as u32, MIN_DIM_SIZE as u32)));
    window.commit();

    let clipboard = unsafe { Clipboard::new(connection.display().id().as_ptr() as *mut _) };

    let pool = SlotPool::new(MIN_DIM_SIZE * MIN_DIM_SIZE * 4, &shm).expect("Failed to create pool");
    let (tx, rx) = sctk::reexports::calloop::channel::sync_channel(10);
    clipboard.init_dnd(Box::new(tx)).expect("Failed to set up DnD");

    _ = event_loop.handle().insert_source(rx, |event, _, state| {
        let calloop::channel::Event::Msg(event) = event else {
            return;
        };
        match event {
            smithay_clipboard::dnd::DndEvent::Offer(id, OfferEvent::Data { data, mime_type }) => {
                let s = smithay_clipboard::text::Text::try_from((data, mime_type)).unwrap();
                println!("Received DnD data for {}: {}", id.unwrap_or_default(), s.0);
            },
            smithay_clipboard::dnd::DndEvent::Offer(id, OfferEvent::Motion { x, y }) => {
                if id != state.offer_hover_id {
                    state.offer_hover_id = id;
                    if let Ok(data) =
                        state.clipboard.peek_offer::<smithay_clipboard::text::Text>(None)
                    {
                        println!("Peeked the data: {}", data.0);
                    }
                }
                println!("Received DnD Motion for {id:?}: at {x}, {y}");
            },

            smithay_clipboard::dnd::DndEvent::Offer(id, OfferEvent::Leave) => {
                if state.internal_dnd {
                    if state.pointer_focus {
                        println!("Internal drop completed!");
                    } else {
                        // Internal DnD will be ignored after leaving the window in which it
                        // started. Another approach might be to allow it to
                        // re-enter before some time has passed.
                        state.internal_dnd = false;
                        state.clipboard.end_dnd();
                    }
                } else {
                    state.offer_hover_id = None;
                    println!("Dnd offer left {id:?}.");
                }
            },
            smithay_clipboard::dnd::DndEvent::Offer(id, OfferEvent::Enter { mime_types, .. }) => {
                println!("Received DnD Enter for {id:?}");
                state.offer_hover_id = id;
                if let Some(mime) = mime_types.first() {
                    if let Ok(data) = state
                        .clipboard
                        .peek_offer::<smithay_clipboard::text::Text>(Some(mime.clone()))
                    {
                        println!("Peeked the data: {}", data.0);
                    }
                }
            },
            smithay_clipboard::dnd::DndEvent::Source(SourceEvent::Finished) => {
                println!("Finished sending data.");
                state.internal_dnd = false;
                state.offer_hover_id = None;
            },
            e => {
                dbg!(e);
            },
        }
    });

    clipboard.register_dnd_destination(window.wl_surface().clone(), vec![
        DndDestinationRectangle {
            id: 0,
            rectangle: Rectangle { x: 0., y: 0., width: 256., height: 256. },
            mime_types: ALLOWED_TEXT_MIME_TYPES
                .iter()
                .map(|m| MimeType::from(Cow::from(m.to_string())))
                .collect(),
            actions: DndAction::all(),
            preferred: DndAction::Copy,
        },
        DndDestinationRectangle {
            id: 1,
            rectangle: Rectangle { x: 256., y: 0., width: 256., height: 256. },
            mime_types: ALLOWED_TEXT_MIME_TYPES
                .iter()
                .map(|m| MimeType::from(Cow::from(m.to_string())))
                .collect(),
            actions: DndAction::all(),
            preferred: DndAction::Copy,
        },
        DndDestinationRectangle {
            id: 2,
            rectangle: Rectangle { x: 0., y: 256., width: 256., height: 256. },
            mime_types: ALLOWED_TEXT_MIME_TYPES
                .iter()
                .map(|m| MimeType::from(Cow::from(m.to_string())))
                .collect(),
            actions: DndAction::Copy,
            preferred: DndAction::Copy,
        },
        DndDestinationRectangle {
            id: 3,
            rectangle: Rectangle { x: 256., y: 256., width: 256., height: 256. },
            mime_types: ALLOWED_TEXT_MIME_TYPES
                .iter()
                .map(|m| MimeType::from(Cow::from(m.to_string())))
                .collect(),
            actions: DndAction::Move,
            preferred: DndAction::Move,
        },
    ]);

    let mut simple_window = SimpleWindow {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &queue_handle),
        output_state: OutputState::new(&globals, &queue_handle),
        shm,
        clipboard,

        exit: false,
        first_configure: true,
        pool,
        width: 256,
        height: 256,
        buffer: None,
        window,
        keyboard: None,
        pointer: None,
        internal_dnd: false,
        keyboard_focus: false,
        pointer_focus: false,
        offer_hover_id: None,
        loop_handle: event_loop.handle(),
    };

    // We don't draw immediately, the configure will notify us when to first draw.
    loop {
        event_loop.dispatch(None, &mut simple_window).unwrap();

        if simple_window.exit {
            break;
        }
    }
}

struct SimpleWindow {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    clipboard: SimpleClipboard,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    internal_dnd: bool,
    keyboard_focus: bool,
    pointer_focus: bool,
    offer_hover_id: Option<u128>,
    loop_handle: LoopHandle<'static, SimpleWindow>,
}

impl CompositorHandler for SimpleWindow {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Not needed for this example.
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // Not needed for this example.
    }

    fn frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(conn, qh);
    }
}

impl OutputHandler for SimpleWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for SimpleWindow {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        println!("Window configured to: {:?}", configure);

        self.buffer = None;
        self.width = configure.new_size.0.map(|v| v.get()).unwrap_or(256);
        self.height = configure.new_size.1.map(|v| v.get()).unwrap_or(256);

        // Initiate the first draw.
        if self.first_configure {
            self.first_configure = false;
            self.draw(conn, qh);
        }
    }
}

impl SeatHandler for SimpleWindow {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            println!("Set keyboard capability");
            let keyboard = self
                .seat_state
                .get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.loop_handle.clone(),
                    Box::new(|_state, _wl_kbd, event| {
                        println!("Repeat: {:?} ", event);
                    }),
                )
                .expect("Failed to create keyboard");

            self.keyboard = Some(keyboard);
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            println!("Set pointer capability");
            let pointer = self.seat_state.get_pointer(qh, &seat).expect("Failed to create pointer");

            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_some() {
            println!("Unset keyboard capability");
            self.keyboard.take().unwrap().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for SimpleWindow {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &sctk::reexports::client::protocol::wl_pointer::WlPointer,
        events: &[sctk::seat::pointer::PointerEvent],
    ) {
        for e in events {
            match &e.kind {
                PointerEventKind::Press { button, .. } if *button == BTN_LEFT => {
                    println!("Starting a drag!");

                    self.clipboard.start_dnd(
                        false,
                        self.window.wl_surface().clone(),
                        Some(Icon::Buf {
                            width: 256,
                            height: 256,
                            data: vec![0x99; 256 * 256 * 4],
                            transparent: true,
                        }),
                        smithay_clipboard::text::Text("Clipboard Drag and Drop!".to_string()),
                        DndAction::all(),
                    );
                },
                PointerEventKind::Press { button, .. } if *button == BTN_RIGHT => {
                    println!("Starting an internal drag!");

                    self.internal_dnd = true;
                    self.clipboard.start_dnd(
                        true,
                        self.window.wl_surface().clone(),
                        Some(Icon::Buf {
                            width: 256,
                            height: 256,
                            data: vec![0xFF; 256 * 256 * 4],
                            transparent: true,
                        }),
                        smithay_clipboard::text::Text(
                            "Internal clipboard Drag and Drop!".to_string(),
                        ),
                        DndAction::all(),
                    );
                },
                PointerEventKind::Leave { .. } => {
                    self.pointer_focus = false;
                },
                PointerEventKind::Enter { .. } => {
                    self.pointer_focus = true;
                },
                _ => {},
            }
        }
    }
}

impl KeyboardHandler for SimpleWindow {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        keysyms: &[Keysym],
    ) {
        if self.window.wl_surface() == surface {
            println!("Keyboard focus on window with pressed syms: {keysyms:?}");
            self.keyboard_focus = true;
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.window.wl_surface() == surface {
            println!("Release keyboard focus on window");
            self.keyboard_focus = false;
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        match event.utf8.as_deref() {
            // Paste primary.
            Some("P") => match self.clipboard.load_primary_text() {
                Ok(contents) => println!("Paste from primary clipboard: {contents}"),
                Err(err) => eprintln!("Error loading from primary clipboard: {err}"),
            },
            // Paste clipboard.
            Some("p") => match self.clipboard.load_text() {
                Ok(contents) => println!("Paste from clipboard: {contents}"),
                Err(err) => eprintln!("Error loading from clipboard: {err}"),
            },
            // Copy primary.
            Some("C") => {
                let to_store = "Copy primary";
                self.clipboard.store_primary_text(to_store);
                println!("Copied string into primary clipboard: {}", to_store);
            },
            // Copy clipboard.
            Some("c") => {
                let to_store = "Copy";
                self.clipboard.store_text(to_store);
                println!("Copied string into clipboard: {}", to_store);
            },
            // Copy URI to primary clipboard.
            Some("F") => {
                let home = Uri::home();
                println!("Copied home dir into primary clipboard: {}", home.0);
                self.clipboard.store_primary(home);
            },
            // Copy URI to clipboard.
            Some("f") => {
                let home = Uri::home();
                println!("Copied home dir into clipboard: {}", home.0);
                self.clipboard.store(home);
            },
            // Read URI from clipboard
            Some("o") => match self.clipboard.load::<Uri>() {
                Ok(uri) => {
                    println!("URI from clipboard: {}", uri.0);
                },
                Err(err) => eprintln!("Error loading from clipboard: {err}"),
            },
            // Read URI from clipboard
            Some("O") => match self.clipboard.load_primary::<Uri>() {
                Ok(uri) => {
                    println!("URI from primary clipboard: {}", uri.0);
                },
                Err(err) => eprintln!("Error loading from clipboard: {err}"),
            },
            _ => (),
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _: u32,
    ) {
    }
}

impl ShmHandler for SimpleWindow {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SimpleWindow {
    pub fn draw(&mut self, _conn: &Connection, qh: &QueueHandle<Self>) {
        let width = self.width;
        let height = self.height;
        let stride = self.width as i32 * 4;

        let buffer = self.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
                .expect("create buffer")
                .0
        });

        let canvas = match self.pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                // This should be rare, but if the compositor has not released the previous
                // buffer, we need double-buffering.
                let (second_buffer, canvas) = self
                    .pool
                    .create_buffer(
                        self.width as i32,
                        self.height as i32,
                        stride,
                        wl_shm::Format::Argb8888,
                    )
                    .expect("create buffer");
                *buffer = second_buffer;
                canvas
            },
        };

        // Draw to the window:
        canvas.chunks_exact_mut(4).for_each(|chunk| {
            // ARGB color.
            let color = 0xFF181818u32;

            let array: &mut [u8; 4] = chunk.try_into().unwrap();
            *array = color.to_le_bytes();
        });

        // Damage the entire window
        self.window.wl_surface().damage_buffer(0, 0, self.width as i32, self.height as i32);

        // Request our next frame
        self.window.wl_surface().frame(qh, self.window.wl_surface().clone());

        // Attach and commit to present.
        buffer.attach_to(self.window.wl_surface()).expect("buffer attach");
        self.window.commit();
    }
}

#[derive(Debug)]
pub struct Uri(Url);

impl Uri {
    pub fn home() -> Self {
        let home = dirs::home_dir().unwrap();
        Uri(Url::from_file_path(home).unwrap())
    }
}

impl AsMimeTypes for Uri {
    fn available(&self) -> Cow<'static, [MimeType]> {
        Self::allowed()
    }

    fn as_bytes(&self, mime_type: &MimeType) -> Option<Cow<'static, [u8]>> {
        if mime_type == &Self::allowed()[0] {
            Some(self.0.to_string().as_bytes().to_vec().into())
        } else {
            None
        }
    }
}

impl AllowedMimeTypes for Uri {
    fn allowed() -> Cow<'static, [MimeType]> {
        std::borrow::Cow::Borrowed(&[MimeType::Other(Cow::Borrowed("text/uri-list"))])
    }
}

#[derive(Error, Debug)]
pub enum UriError {
    #[error("Unsupported mime type")]
    Unsupported,
    #[error("Utf8 error")]
    Utf8(Utf8Error),
    #[error("URL parse error")]
    Parse(url::ParseError),
}

impl TryFrom<(Vec<u8>, MimeType)> for Uri {
    type Error = UriError;

    fn try_from((data, mime): (Vec<u8>, MimeType)) -> Result<Self, Self::Error> {
        if mime == Self::allowed()[0] {
            std::str::from_utf8(&data)
                .map_err(UriError::Utf8)
                .and_then(|s| Url::from_str(s).map_err(UriError::Parse))
                .map(Uri)
        } else {
            Err(UriError::Unsupported)
        }
    }
}

pub const URI_MIME_TYPE: &str = "text/uri-list";

delegate_compositor!(SimpleWindow);
delegate_output!(SimpleWindow);
delegate_shm!(SimpleWindow);

delegate_seat!(SimpleWindow);
delegate_keyboard!(SimpleWindow);
delegate_pointer!(SimpleWindow);

delegate_xdg_shell!(SimpleWindow);
delegate_xdg_window!(SimpleWindow);

delegate_registry!(SimpleWindow);

impl ProvidesRegistryState for SimpleWindow {
    registry_handlers![OutputState, SeatState,];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}
