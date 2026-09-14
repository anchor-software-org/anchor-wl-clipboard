//! Clipboard access via Wayland data-control with an X11/XWayland fallback.
//!
//! Zero external binary dependencies. Provides ClipboardWatcher (monitors
//! clipboard changes) and ClipboardWriter (sets clipboard content).

use std::io::{Read, Write as IoWrite};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

// ── Public types ────────────────────────────────────────────────────────

/// Errors returned by the library.
#[derive(Debug)]
pub enum Error {
    WaylandConnect(String),
    X11Connect(String),
    NoBackend { wayland: String, x11: String },
    UnsupportedProtocol,
    Io(std::io::Error),
    WatcherDead,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::WaylandConnect(s) => write!(f, "Wayland connect: {}", s),
            Error::X11Connect(s) => write!(f, "X11 connect: {}", s),
            Error::NoBackend { wayland, x11 } => {
                write!(f, "no clipboard backend available (Wayland: {wayland}; X11: {x11})")
            }
            Error::UnsupportedProtocol => {
                write!(f, "Compositor does not support zwlr_data_control_manager_v1")
            }
            Error::Io(e) => write!(f, "I/O: {}", e),
            Error::WatcherDead => write!(f, "Watcher thread stopped"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// MIME types we handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MimeType {
    TextPlain,
    TextPlainUtf8,
    ImagePng,
    Custom(String),
}

impl MimeType {
    pub fn as_str(&self) -> &str {
        match self {
            MimeType::TextPlain => "text/plain",
            MimeType::TextPlainUtf8 => "text/plain;charset=utf-8",
            MimeType::ImagePng => "image/png",
            MimeType::Custom(s) => s.as_str(),
        }
    }

    pub fn from_mime(s: &str) -> Self {
        match s {
            "text/plain" => MimeType::TextPlain,
            "text/plain;charset=utf-8" | "text/plain; charset=utf-8" => MimeType::TextPlainUtf8,
            "image/png" => MimeType::ImagePng,
            other => MimeType::Custom(other.to_string()),
        }
    }
}

/// A snapshot of clipboard content.
#[derive(Debug, Clone)]
pub struct ClipboardContent {
    pub data: Vec<u8>,
    pub mime_type: MimeType,
    pub available_types: Vec<String>,
}

/// Events emitted by the clipboard watcher.
#[derive(Debug)]
pub enum ClipboardEvent {
    Changed(ClipboardContent),
    Cleared,
    Error(Error),
}

// ── Watcher ─────────────────────────────────────────────────────────────

pub struct ClipboardWatcherBuilder {
    mime_preferences: Vec<String>,
    watch_primary: bool,
}

impl ClipboardWatcherBuilder {
    pub fn new() -> Self {
        ClipboardWatcherBuilder {
            mime_preferences: vec![
                "text/plain;charset=utf-8".into(),
                "text/plain".into(),
                "image/png".into(),
            ],
            watch_primary: false,
        }
    }

    pub fn mime_preferences(mut self, prefs: Vec<String>) -> Self {
        self.mime_preferences = prefs;
        self
    }

    pub fn watch_primary(mut self, watch: bool) -> Self {
        self.watch_primary = watch;
        self
    }

    pub fn build(self) -> Result<ClipboardWatcher, Error> {
        let mime_preferences = self.mime_preferences.clone();
        let watch_primary = self.watch_primary;
        match self.build_wayland() {
            Ok(watcher) => Ok(watcher),
            Err(wayland_error) => {
                log::info!(
                    "wlroots data-control clipboard unavailable ({wayland_error}); trying ext-data-control"
                );
                match build_extended_wayland_watcher(mime_preferences.clone(), watch_primary) {
                    Ok(watcher) => Ok(watcher),
                    Err(extended_error) => {
                        log::info!(
                            "Standard Wayland data-control clipboard unavailable ({extended_error}); trying X11/XWayland"
                        );
                        build_x11_watcher(mime_preferences, watch_primary).map_err(|x11_error| {
                            Error::NoBackend {
                                wayland: format!("wlr: {wayland_error}; ext/wlr: {extended_error}"),
                                x11: x11_error.to_string(),
                            }
                        })
                    }
                }
            }
        }
    }

    fn build_wayland(self) -> Result<ClipboardWatcher, Error> {
        let conn =
            Connection::connect_to_env().map_err(|e| Error::WaylandConnect(format!("{}", e)))?;

        let display = conn.display();

        let (event_tx, event_rx) = mpsc::channel::<ClipboardEvent>();

        let state = Arc::new(Mutex::new(WatcherState {
            seat: None,
            manager: None,
            device: None,
            pending_offer: None,
            current_offer: None,
            mime_preferences: self.mime_preferences,
            watch_primary: self.watch_primary,
            event_tx: event_tx.clone(),
        }));

        let mut event_queue: EventQueue<WatcherDispatch> = conn.new_event_queue();
        let qh = event_queue.handle();

        // Initial roundtrip to discover globals
        display.get_registry(&qh, ());
        event_queue
            .roundtrip(&mut WatcherDispatch { state: state.clone() })
            .map_err(|e| Error::WaylandConnect(format!("roundtrip: {}", e)))?;

        // Check we found both seat and manager
        {
            let s = state.lock().unwrap();
            if s.manager.is_none() {
                return Err(Error::UnsupportedProtocol);
            }
            if s.seat.is_none() {
                return Err(Error::WaylandConnect("No wl_seat found".into()));
            }
        }

        // Create data control device
        {
            let mut s = state.lock().unwrap();
            let manager = s.manager.as_ref().unwrap().clone();
            let seat = s.seat.as_ref().unwrap().clone();
            let device = manager.get_data_device(&seat, &qh, ());
            s.device = Some(device);
        }

        // Second roundtrip to process device events
        event_queue
            .roundtrip(&mut WatcherDispatch { state: state.clone() })
            .map_err(|e| Error::WaylandConnect(format!("roundtrip2: {}", e)))?;

        let shutdown = Arc::new(Mutex::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = thread::Builder::new()
            .name("wl-clipboard-watcher".into())
            .spawn(move || {
                let mut dispatch = WatcherDispatch { state: state.clone() };
                loop {
                    if *shutdown_clone.lock().unwrap() {
                        break;
                    }
                    match event_queue.blocking_dispatch(&mut dispatch) {
                        Ok(_) => {}
                        Err(e) => {
                            let _ = dispatch.state.lock().unwrap().event_tx.send(
                                ClipboardEvent::Error(Error::WaylandConnect(format!(
                                    "dispatch: {}",
                                    e
                                ))),
                            );
                            break;
                        }
                    }
                }
            })
            .map_err(Error::Io)?;

        Ok(ClipboardWatcher { rx: event_rx, _handle: Some(handle), _shutdown: shutdown })
    }
}

impl Default for ClipboardWatcherBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn build_extended_wayland_watcher(
    mime_preferences: Vec<String>,
    watch_primary: bool,
) -> Result<ClipboardWatcher, Error> {
    use wayland_clipboard_backend::paste::{
        ClipboardType, Error as PasteError, Seat, get_mime_types,
    };

    let clipboard_type =
        if watch_primary { ClipboardType::Primary } else { ClipboardType::Regular };
    match get_mime_types(clipboard_type, Seat::Unspecified) {
        Ok(_) | Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {}
        Err(error) => return Err(Error::WaylandConnect(error.to_string())),
    }

    let (event_tx, event_rx) = mpsc::channel();
    let shutdown = Arc::new(Mutex::new(false));
    let watcher_shutdown = shutdown.clone();
    let handle = thread::Builder::new()
        .name("ext-clipboard-watcher".into())
        .spawn(move || {
            use wayland_clipboard_backend::paste::{MimeType as PasteMimeType, get_contents};

            let mut previous: Option<(MimeType, Vec<u8>)> = None;
            while !*watcher_shutdown.lock().unwrap() {
                let preferred = mime_preferences
                    .iter()
                    .find(|mime| mime.as_str() == "image/png")
                    .map(|_| PasteMimeType::Any)
                    .unwrap_or(PasteMimeType::Text);
                match get_contents(clipboard_type, Seat::Unspecified, preferred) {
                    Ok((mut reader, actual_mime)) => {
                        let mut data = Vec::new();
                        if let Err(error) = reader.read_to_end(&mut data) {
                            log::debug!("Could not read Wayland clipboard offer: {error}");
                        } else if !data.is_empty() {
                            let mime_type = normalize_wayland_mime(&actual_mime);
                            let changed = previous
                                .as_ref()
                                .map(|(old_mime, old_data)| {
                                    old_mime != &mime_type || old_data != &data
                                })
                                .unwrap_or(true);
                            if changed {
                                let event_data = data.clone();
                                previous = Some((mime_type.clone(), data));
                                if event_tx
                                    .send(ClipboardEvent::Changed(ClipboardContent {
                                        data: event_data,
                                        mime_type,
                                        available_types: vec![actual_mime],
                                    }))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {
                        if previous.take().is_some()
                            && event_tx.send(ClipboardEvent::Cleared).is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = event_tx
                            .send(ClipboardEvent::Error(Error::WaylandConnect(error.to_string())));
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
        .map_err(Error::Io)?;

    log::info!("Using ext-data-control Wayland clipboard watcher");
    Ok(ClipboardWatcher { rx: event_rx, _handle: Some(handle), _shutdown: shutdown })
}

fn normalize_wayland_mime(mime: &str) -> MimeType {
    match mime {
        "UTF8_STRING" | "STRING" | "TEXT" | "text/plain;charset=utf-8" => MimeType::TextPlainUtf8,
        "text/plain" => MimeType::TextPlain,
        "image/png" => MimeType::ImagePng,
        other => MimeType::Custom(other.to_string()),
    }
}

fn build_x11_watcher(
    mime_preferences: Vec<String>,
    watch_primary: bool,
) -> Result<ClipboardWatcher, Error> {
    let clipboard =
        x11_clipboard::Clipboard::new().map_err(|error| Error::X11Connect(error.to_string()))?;
    let selection = if watch_primary {
        clipboard.getter.atoms.primary
    } else {
        clipboard.getter.atoms.clipboard
    };
    let property = clipboard.getter.atoms.property;
    let targets = mime_preferences
        .into_iter()
        .filter_map(|mime| {
            let mime_type = MimeType::from_mime(&mime);
            let target = match mime_type {
                MimeType::TextPlain | MimeType::TextPlainUtf8 => clipboard.getter.atoms.utf8_string,
                _ => match clipboard.getter.get_atom(&mime) {
                    Ok(target) => target,
                    Err(error) => {
                        log::debug!("Could not register X11 clipboard target {mime}: {error}");
                        return None;
                    }
                },
            };
            Some((mime, mime_type, target))
        })
        .collect::<Vec<_>>();
    let (event_tx, event_rx) = mpsc::channel();
    let shutdown = Arc::new(Mutex::new(false));
    let watcher_shutdown = shutdown.clone();
    let handle = thread::Builder::new()
        .name("x11-clipboard-watcher".into())
        .spawn(move || {
            let mut previous: Option<(MimeType, Vec<u8>)> = None;
            while !*watcher_shutdown.lock().unwrap() {
                let current = targets.iter().find_map(|(mime, mime_type, target)| {
                    clipboard
                        .load(selection, *target, property, Duration::from_millis(100))
                        .ok()
                        .filter(|data| !data.is_empty())
                        .map(|data| (mime.clone(), mime_type.clone(), data))
                });

                if let Some((mime, mime_type, data)) = current {
                    let changed = previous
                        .as_ref()
                        .map(|(old_mime, old_data)| old_mime != &mime_type || old_data != &data)
                        .unwrap_or(true);
                    if changed {
                        let event_data = data.clone();
                        previous = Some((mime_type.clone(), data));
                        if event_tx
                            .send(ClipboardEvent::Changed(ClipboardContent {
                                data: event_data,
                                mime_type,
                                available_types: vec![mime],
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(150));
            }
        })
        .map_err(Error::Io)?;

    log::info!("Using X11/XWayland clipboard watcher fallback");
    Ok(ClipboardWatcher { rx: event_rx, _handle: Some(handle), _shutdown: shutdown })
}

pub struct ClipboardWatcher {
    pub rx: Receiver<ClipboardEvent>,
    _handle: Option<JoinHandle<()>>,
    _shutdown: Arc<Mutex<bool>>,
}

impl ClipboardWatcher {
    pub fn stop(self) {
        *self._shutdown.lock().unwrap() = true;
        // Handle will be joined on drop
    }
}

// ── Watcher internals ───────────────────────────────────────────────────

struct PendingOffer {
    offer: zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    mime_types: Vec<String>,
}

struct WatcherState {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    pending_offer: Option<PendingOffer>,
    current_offer: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
    mime_preferences: Vec<String>,
    watch_primary: bool,
    event_tx: Sender<ClipboardEvent>,
}

struct WatcherDispatch {
    state: Arc<Mutex<WatcherState>>,
}

// Registry handler -- discover seat and data control manager
impl Dispatch<wl_registry::WlRegistry, ()> for WatcherDispatch {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            let mut s = state.state.lock().unwrap();
            match interface.as_str() {
                "wl_seat" => {
                    if s.seat.is_none() {
                        let seat =
                            registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
                        s.seat = Some(seat);
                    }
                }
                "zwlr_data_control_manager_v1" => {
                    let mgr = registry
                        .bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(
                            name,
                            version.min(2),
                            qh,
                            (),
                        );
                    s.manager = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

// Seat -- we don't need seat events, just the object
delegate_noop!(WatcherDispatch: ignore wl_seat::WlSeat);

// Manager -- no events
delegate_noop!(WatcherDispatch: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

// Data control device -- selection events
impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for WatcherDispatch {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut s = state.state.lock().unwrap();
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                // New offer being described -- replace any previous pending
                s.pending_offer = Some(PendingOffer { offer: id, mime_types: Vec::new() });
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                handle_selection(&mut s, id, false);
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                if s.watch_primary {
                    handle_selection(&mut s, id, true);
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                log::info!("Data control device finished");
            }
            _ => {}
        }
    }

    // The data_offer event (opcode 0) creates a new zwlr_data_control_offer_v1 object.
    // We must tell wayland-client how to initialize it, otherwise the default impl panics.
    event_created_child!(WatcherDispatch, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        0 => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

fn handle_selection(
    s: &mut WatcherState,
    id: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
    _is_primary: bool,
) {
    // Destroy old offer
    if let Some(old) = s.current_offer.take() {
        old.destroy();
    }

    match id {
        None => {
            // Clipboard was cleared
            s.pending_offer = None;
            let _ = s.event_tx.send(ClipboardEvent::Cleared);
        }
        Some(offer_proxy) => {
            // Find the pending offer that matches this proxy
            if let Some(pending) = s.pending_offer.take() {
                let available_types = pending.mime_types.clone();

                // Find best matching MIME type
                let best_mime = s
                    .mime_preferences
                    .iter()
                    .find(|pref| available_types.iter().any(|t| t == *pref))
                    .cloned();

                if let Some(mime_str) = best_mime {
                    // Create a pipe and request the content
                    match os_pipe::pipe() {
                        Ok((read_end, write_end)) => {
                            pending.offer.receive(mime_str.clone(), write_end.as_fd());
                            // IMPORTANT: drop write end so we can read EOF
                            drop(write_end);

                            let mime_type = MimeType::from_mime(&mime_str);
                            let tx = s.event_tx.clone();

                            // Read in a separate thread to not block Wayland dispatch
                            thread::spawn(move || {
                                let mut buf = Vec::new();
                                let mut reader = read_end;
                                match reader.read_to_end(&mut buf) {
                                    Ok(_) => {
                                        let _ =
                                            tx.send(ClipboardEvent::Changed(ClipboardContent {
                                                data: buf,
                                                mime_type,
                                                available_types,
                                            }));
                                    }
                                    Err(e) => {
                                        log::warn!("Clipboard pipe read error: {}", e);
                                        // Still send partial data if we have any
                                        if !buf.is_empty() {
                                            let _ = tx.send(ClipboardEvent::Changed(
                                                ClipboardContent {
                                                    data: buf,
                                                    mime_type,
                                                    available_types,
                                                },
                                            ));
                                        }
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            log::error!("Failed to create pipe: {}", e);
                        }
                    }
                } else {
                    log::debug!("No matching MIME type in offer. Available: {:?}", available_types);
                }

                s.current_offer = Some(offer_proxy);
            }
        }
    }
}

// Data control offer -- accumulate MIME types
impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for WatcherDispatch {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            let mut s = state.state.lock().unwrap();
            if let Some(ref mut pending) = s.pending_offer {
                pending.mime_types.push(mime_type);
            }
        }
    }
}

// ── Writer ──────────────────────────────────────────────────────────────

pub struct ClipboardWriter {
    cmd_tx: Sender<WriterCommand>,
    _handle: JoinHandle<()>,
}

enum WriterCommand {
    SetText(String),
    SetImagePng(Vec<u8>),
    Shutdown,
}

struct WriterState {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    cmd_rx: Option<Receiver<WriterCommand>>,
}

#[derive(Clone)]
struct WriterContent {
    data: Vec<u8>,
    mime_types: Vec<String>,
}

struct WriterDispatch {
    state: Arc<Mutex<WriterState>>,
    pending_content: Arc<Mutex<Option<WriterContent>>>,
}

impl ClipboardWriter {
    pub fn new() -> Result<Self, Error> {
        match Self::new_wayland() {
            Ok(writer) => Ok(writer),
            Err(wayland_error) => {
                log::info!(
                    "wlroots data-control clipboard writer unavailable ({wayland_error}); trying ext-data-control"
                );
                match Self::new_extended_wayland() {
                    Ok(writer) => Ok(writer),
                    Err(extended_error) => {
                        log::info!(
                            "Standard Wayland data-control writer unavailable ({extended_error}); trying X11/XWayland"
                        );
                        Self::new_x11().map_err(|x11_error| Error::NoBackend {
                            wayland: format!("wlr: {wayland_error}; ext/wlr: {extended_error}"),
                            x11: x11_error.to_string(),
                        })
                    }
                }
            }
        }
    }

    fn new_wayland() -> Result<Self, Error> {
        let conn =
            Connection::connect_to_env().map_err(|e| Error::WaylandConnect(format!("{}", e)))?;

        let display = conn.display();
        let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCommand>();

        let pending_content: Arc<Mutex<Option<WriterContent>>> = Arc::new(Mutex::new(None));

        let state = Arc::new(Mutex::new(WriterState {
            seat: None,
            manager: None,
            device: None,
            cmd_rx: Some(cmd_rx),
        }));

        let mut event_queue: EventQueue<WriterDispatch> = conn.new_event_queue();
        let qh = event_queue.handle();

        display.get_registry(&qh, ());

        let mut dispatch =
            WriterDispatch { state: state.clone(), pending_content: pending_content.clone() };

        event_queue
            .roundtrip(&mut dispatch)
            .map_err(|e| Error::WaylandConnect(format!("roundtrip: {}", e)))?;

        // Check globals
        {
            let s = state.lock().unwrap();
            if s.manager.is_none() {
                return Err(Error::UnsupportedProtocol);
            }
            if s.seat.is_none() {
                return Err(Error::WaylandConnect("No wl_seat found".into()));
            }
        }

        // Create device
        {
            let mut s = state.lock().unwrap();
            let manager = s.manager.as_ref().unwrap().clone();
            let seat = s.seat.as_ref().unwrap().clone();
            let device = manager.get_data_device(&seat, &qh, ());
            s.device = Some(device);
        }

        event_queue
            .roundtrip(&mut dispatch)
            .map_err(|e| Error::WaylandConnect(format!("roundtrip2: {}", e)))?;

        let handle = thread::Builder::new()
            .name("wl-clipboard-writer".into())
            .spawn(move || {
                writer_loop(event_queue, dispatch, state, qh);
            })
            .map_err(Error::Io)?;

        Ok(ClipboardWriter { cmd_tx, _handle: handle })
    }

    fn new_x11() -> Result<Self, Error> {
        let clipboard = x11_clipboard::Clipboard::new()
            .map_err(|error| Error::X11Connect(error.to_string()))?;
        let png_atom = clipboard
            .setter
            .get_atom("image/png")
            .map_err(|error| Error::X11Connect(error.to_string()))?;
        let selection = clipboard.setter.atoms.clipboard;
        let text_atom = clipboard.setter.atoms.utf8_string;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("x11-clipboard-writer".into())
            .spawn(move || {
                while let Ok(command) = cmd_rx.recv() {
                    let result = match command {
                        WriterCommand::SetText(text) => {
                            clipboard.store(selection, text_atom, text.into_bytes())
                        }
                        WriterCommand::SetImagePng(data) => {
                            clipboard.store(selection, png_atom, data)
                        }
                        WriterCommand::Shutdown => break,
                    };
                    if let Err(error) = result {
                        log::error!("Could not publish X11/XWayland clipboard content: {error}");
                    }
                }
            })
            .map_err(Error::Io)?;
        log::info!("Using X11/XWayland clipboard writer fallback");
        Ok(Self { cmd_tx, _handle: handle })
    }

    fn new_extended_wayland() -> Result<Self, Error> {
        use wayland_clipboard_backend::paste::{
            ClipboardType, Error as PasteError, Seat, get_mime_types,
        };

        match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(_) | Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => {}
            Err(error) => return Err(Error::WaylandConnect(error.to_string())),
        }

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("ext-clipboard-writer".into())
            .spawn(move || {
                use wayland_clipboard_backend::copy::{MimeType, Options, Source};

                while let Ok(command) = cmd_rx.recv() {
                    let result = match command {
                        WriterCommand::SetText(text) => Options::new()
                            .copy(Source::Bytes(text.into_bytes().into()), MimeType::Text),
                        WriterCommand::SetImagePng(data) => Options::new().copy(
                            Source::Bytes(data.into()),
                            MimeType::Specific("image/png".into()),
                        ),
                        WriterCommand::Shutdown => break,
                    };
                    if let Err(error) = result {
                        log::error!(
                            "Could not publish ext-data-control clipboard content: {error}"
                        );
                    }
                }
            })
            .map_err(Error::Io)?;
        log::info!("Using ext-data-control Wayland clipboard writer");
        Ok(Self { cmd_tx, _handle: handle })
    }

    pub fn set_text(&self, text: &str) -> Result<(), Error> {
        self.cmd_tx.send(WriterCommand::SetText(text.to_string())).map_err(|_| Error::WatcherDead)
    }

    pub fn set_image_png(&self, data: &[u8]) -> Result<(), Error> {
        self.cmd_tx.send(WriterCommand::SetImagePng(data.to_vec())).map_err(|_| Error::WatcherDead)
    }
}

impl Drop for ClipboardWriter {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WriterCommand::Shutdown);
    }
}

fn writer_loop(
    mut event_queue: EventQueue<WriterDispatch>,
    mut dispatch: WriterDispatch,
    state: Arc<Mutex<WriterState>>,
    qh: QueueHandle<WriterDispatch>,
) {
    let cmd_rx = state.lock().unwrap().cmd_rx.take().unwrap();

    loop {
        // Process any pending Wayland events (non-blocking)
        let _ = event_queue.dispatch_pending(&mut dispatch);

        // Check for commands with a timeout so we keep dispatching Wayland events
        match cmd_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(WriterCommand::SetText(text)) => {
                let content = WriterContent {
                    data: text.into_bytes(),
                    mime_types: vec!["text/plain;charset=utf-8".into(), "text/plain".into()],
                };
                set_selection(&state, &qh, &dispatch.pending_content, content);
                let _ = event_queue.flush();
            }
            Ok(WriterCommand::SetImagePng(data)) => {
                let content = WriterContent { data, mime_types: vec!["image/png".into()] };
                set_selection(&state, &qh, &dispatch.pending_content, content);
                let _ = event_queue.flush();
            }
            Ok(WriterCommand::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Just keep dispatching
                let _ = event_queue.flush();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn set_selection(
    state: &Arc<Mutex<WriterState>>,
    qh: &QueueHandle<WriterDispatch>,
    pending_content: &Arc<Mutex<Option<WriterContent>>>,
    content: WriterContent,
) {
    let s = state.lock().unwrap();
    let manager = match s.manager.as_ref() {
        Some(m) => m,
        None => return,
    };
    let device = match s.device.as_ref() {
        Some(d) => d,
        None => return,
    };

    // Create a data source
    let source = manager.create_data_source(qh, ());

    // Offer our MIME types
    for mime in &content.mime_types {
        source.offer(mime.clone());
    }

    // Store content for when compositor asks us to write
    *pending_content.lock().unwrap() = Some(content);

    // Set the selection
    device.set_selection(Some(&source));
}

// Writer registry handler
impl Dispatch<wl_registry::WlRegistry, ()> for WriterDispatch {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            let mut s = state.state.lock().unwrap();
            match interface.as_str() {
                "wl_seat" => {
                    if s.seat.is_none() {
                        let seat =
                            registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
                        s.seat = Some(seat);
                    }
                }
                "zwlr_data_control_manager_v1" => {
                    let mgr = registry
                        .bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(
                            name,
                            version.min(2),
                            qh,
                            (),
                        );
                    s.manager = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(WriterDispatch: ignore wl_seat::WlSeat);
delegate_noop!(WriterDispatch: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

// Writer device -- we don't use device events for writing, but need to handle them
impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for WriterDispatch {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        _event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Writer ignores device events
    }

    // The data_offer event (opcode 0) creates a new zwlr_data_control_offer_v1 object.
    event_created_child!(WriterDispatch, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        0 => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

// Writer doesn't watch offers
impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for WriterDispatch {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        _event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Ignore
    }
}

// Data source -- handle send and cancelled events
impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for WriterDispatch {
    fn event(
        state: &mut Self,
        source: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                // Compositor is asking us to write our content to fd
                let content = state.pending_content.lock().unwrap().clone();
                if let Some(content) = content
                    && content.mime_types.contains(&mime_type)
                {
                    let data = content.data.clone();
                    let raw_fd = fd.as_raw_fd();
                    // Need to transfer ownership of the fd
                    std::mem::forget(fd);
                    thread::spawn(move || {
                        let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
                        let _ = file.write_all(&data);
                        // file is dropped here, closing the fd
                    });
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                log::debug!("Clipboard source cancelled");
                source.destroy();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_type_roundtrip() {
        assert_eq!(MimeType::from_mime("text/plain"), MimeType::TextPlain);
        assert_eq!(MimeType::from_mime("text/plain;charset=utf-8"), MimeType::TextPlainUtf8);
        assert_eq!(MimeType::from_mime("image/png"), MimeType::ImagePng);
        assert_eq!(
            MimeType::from_mime("application/json"),
            MimeType::Custom("application/json".into())
        );
    }

    #[test]
    fn mime_type_as_str() {
        assert_eq!(MimeType::TextPlain.as_str(), "text/plain");
        assert_eq!(MimeType::TextPlainUtf8.as_str(), "text/plain;charset=utf-8");
        assert_eq!(MimeType::ImagePng.as_str(), "image/png");
    }

    #[test]
    fn normalizes_wayland_text_aliases() {
        for mime in ["UTF8_STRING", "STRING", "TEXT", "text/plain;charset=utf-8"] {
            assert_eq!(normalize_wayland_mime(mime), MimeType::TextPlainUtf8);
        }
        assert_eq!(normalize_wayland_mime("text/plain"), MimeType::TextPlain);
        assert_eq!(normalize_wayland_mime("image/png"), MimeType::ImagePng);
    }
}
