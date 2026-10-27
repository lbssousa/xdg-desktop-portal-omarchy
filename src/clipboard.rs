//! The seat's clipboard through ext-data-control (which Hyprland offers to
//! unsandboxed clients such as this one), on behalf of portal sessions.
//!
//! An app's selection is owned here, by a data source offering the app's
//! MIME types. The first time a type is requested, the app is asked for the
//! content (SelectionTransfer); what it writes is kept and served to that
//! reader and every later one, until another selection replaces this one.
//! Bitwarden, for one, answers a single transfer and closes its session,
//! and clipboard watchers (wl-paste --watch in Omarchy's clipboard history)
//! read each new selection at once — without the cache, they would use up
//! the only transfer before the user pastes. The cache outlives the session
//! for the same reason.
//!
//! Sessions of apps listed as sensitive also offer x-kde-passwordManagerHint
//! ("secret"), which clipboard histories (Omarchy's, KDE's, cliphist) skip.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use nix::fcntl::OFlag;
use nix::unistd::pipe2;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, warn};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::secret::Secret;

pub const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";
const MAX_MIME_TYPES: usize = 64;
const MAX_MIME_LEN: usize = 256;
const MAX_CONTENT_BYTES: usize = 64 << 20;

/// What the D-Bus side has to signal to the frontend.
#[derive(Debug, PartialEq)]
pub enum Event {
    Transfer {
        session: OwnedObjectPath,
        mime_type: String,
        serial: u32,
    },
    OwnerChanged {
        session: OwnedObjectPath,
        mime_types: Vec<String>,
        is_owner: bool,
    },
}

#[derive(Debug)]
pub enum Error {
    NoSession,
    NotEnabled,
    NoSuchTransfer,
    NotAvailable,
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoSession => f.write_str("no such session"),
            Error::NotEnabled => f.write_str("the clipboard isn't enabled for this session"),
            Error::NoSuchTransfer => f.write_str("no such transfer"),
            Error::NotAvailable => f.write_str("the clipboard has no content of that type"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<nix::Error> for Error {
    fn from(e: nix::Error) -> Self {
        Error::Io(e.into())
    }
}

/// A RemoteDesktop session, as far as the clipboard goes.
#[derive(Debug, Default)]
pub struct Session {
    pub app_id: String,
    pub clipboard_requested: bool,
    pub clipboard_enabled: bool,
    pub sensitive: bool,
    /// From SelectDevices: what the app asked for, and whether the
    /// frontend handed back a restore_data granting the clipboard.
    pub persist_mode: u32,
    pub restored: bool,
}

/// The selection this backend owns for an app.
struct Owned {
    source: ExtDataControlSourceV1,
    generation: u64,
    /// None once the app's session closed; the cache keeps serving.
    session: Option<OwnedObjectPath>,
    sensitive: bool,
    cache: HashMap<String, Secret>,
    /// Readers waiting for a type the app is being asked for.
    waiting: HashMap<String, Vec<OwnedFd>>,
    /// The next selection event is the offer for this source.
    awaiting_own_offer: bool,
}

/// A SelectionTransfer the app hasn't finished yet.
struct Transfer {
    session: OwnedObjectPath,
    mime_type: String,
    generation: u64,
    writing: bool,
    /// What the app wrote (Err: unreadable or too large).
    read: Option<Result<Secret, ()>>,
    /// SelectionWriteDone's success.
    done: Option<bool>,
}

pub struct Clipboard {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    offers: HashMap<ObjectId, Vec<String>>,
    selection: Option<ExtDataControlOfferV1>,
    owned: Option<Owned>,
    next_generation: u64,
    transfers: HashMap<u32, Transfer>,
    next_serial: u32,
    pub sessions: HashMap<OwnedObjectPath, Session>,
    events: UnboundedSender<Event>,
}

pub type Shared = Arc<Mutex<Clipboard>>;

pub fn lock(shared: &Shared) -> MutexGuard<'_, Clipboard> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// The Wayland event queue's state: just the way back to the clipboard.
pub struct State {
    shared: Shared,
}

pub struct Wayland {
    queue: EventQueue<State>,
    state: State,
}

impl Wayland {
    /// Dispatches Wayland events until the connection fails.
    pub fn run(mut self) -> Result<(), wayland_client::DispatchError> {
        loop {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
    }
}

/// Connects to the compositor ($WAYLAND_DISPLAY) and takes the first
/// seat's data device.
pub fn connect(
    events: UnboundedSender<Event>,
) -> Result<(Shared, Wayland), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("ext_data_control_manager_v1: {e}"))?;
    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;
    let device = manager.get_data_device(&seat, &qh, ());
    let clipboard = Clipboard {
        conn,
        qh,
        manager,
        device,
        offers: HashMap::new(),
        selection: None,
        owned: None,
        next_generation: 1,
        transfers: HashMap::new(),
        next_serial: 1,
        sessions: HashMap::new(),
        events,
    };
    let shared = Arc::new(Mutex::new(clipboard));
    let mut state = State {
        shared: shared.clone(),
    };
    queue.roundtrip(&mut state)?;
    Ok((shared, Wayland { queue, state }))
}

impl Clipboard {
    fn enabled(&self, session: &ObjectPath<'_>) -> Result<&Session, Error> {
        let s = self.sessions.get(session).ok_or(Error::NoSession)?;
        if !s.clipboard_enabled {
            return Err(Error::NotEnabled);
        }
        Ok(s)
    }

    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            warn!("flushing the Wayland connection: {e}");
        }
    }

    /// Makes `mime_types` (the app's content) the selection; an empty list
    /// clears it.
    pub fn set_selection(
        &mut self,
        session: &ObjectPath<'_>,
        mime_types: Vec<String>,
    ) -> Result<(), Error> {
        let sensitive = self.enabled(session)?.sensitive;
        let mime_types = clean_mime_types(mime_types);
        self.drop_owned();
        if mime_types.is_empty() {
            self.device.set_selection(None);
            self.flush();
            return Ok(());
        }
        let source = self.manager.create_data_source(&self.qh, ());
        for mime in &mime_types {
            source.offer(mime.clone());
        }
        if sensitive {
            source.offer(PASSWORD_HINT.to_string());
        }
        self.device.set_selection(Some(&source));
        debug!("{session}: selection set ({mime_types:?})");
        let generation = self.next_generation;
        self.next_generation += 1;
        self.owned = Some(Owned {
            source,
            generation,
            session: Some(session.to_owned().into()),
            sensitive,
            cache: HashMap::new(),
            waiting: HashMap::new(),
            awaiting_own_offer: true,
        });
        self.flush();
        Ok(())
    }

    /// The pipe the app writes the content of a transfer to.
    pub fn selection_write(
        &mut self,
        shared: &Shared,
        session: &ObjectPath<'_>,
        serial: u32,
    ) -> Result<OwnedFd, Error> {
        let transfer = self
            .transfers
            .get_mut(&serial)
            .filter(|t| t.session.as_str() == session.as_str() && !t.writing)
            .ok_or(Error::NoSuchTransfer)?;
        let (read, write) = pipe2(OFlag::O_CLOEXEC)?;
        transfer.writing = true;
        let shared = shared.clone();
        thread::spawn(move || {
            let content = read_secret(read);
            lock(&shared).transfer_read(serial, content);
        });
        Ok(write)
    }

    pub fn selection_write_done(
        &mut self,
        session: &ObjectPath<'_>,
        serial: u32,
        success: bool,
    ) -> Result<(), Error> {
        let transfer = self
            .transfers
            .get_mut(&serial)
            .filter(|t| t.session.as_str() == session.as_str() && t.done.is_none())
            .ok_or(Error::NoSuchTransfer)?;
        transfer.done = Some(success);
        if !success || transfer.read.is_some() {
            self.finish(serial);
        }
        Ok(())
    }

    fn transfer_read(&mut self, serial: u32, content: Result<Secret, ()>) {
        let Some(transfer) = self.transfers.get_mut(&serial) else {
            return;
        };
        transfer.read = Some(content);
        if transfer.done.is_some() {
            self.finish(serial);
        }
    }

    /// Hands the app's content to the waiting readers and caches it, or,
    /// if the transfer failed, closes their pipes empty.
    fn finish(&mut self, serial: u32) {
        let Some(transfer) = self.transfers.remove(&serial) else {
            return;
        };
        let Some(owned) = self
            .owned
            .as_mut()
            .filter(|o| o.generation == transfer.generation)
        else {
            return;
        };
        let waiting = owned
            .waiting
            .remove(&transfer.mime_type)
            .unwrap_or_default();
        match (transfer.done, transfer.read) {
            (Some(true), Some(Ok(content))) => {
                for fd in waiting {
                    write_async(fd, content.copy());
                }
                owned.cache.insert(transfer.mime_type, content);
            }
            _ => debug!("transfer {serial} failed; its readers get nothing"),
        }
    }

    /// The current selection's content as `mime_type`, through a pipe.
    pub fn selection_read(
        &mut self,
        session: &ObjectPath<'_>,
        mime_type: &str,
    ) -> Result<OwnedFd, Error> {
        self.enabled(session)?;
        if let Some(owned) = &self.owned {
            // Reading back our own selection must not ask the app (which
            // may be the one waiting on this read), so only what it
            // already handed over is available.
            let content = owned.cache.get(mime_type).ok_or(Error::NotAvailable)?;
            let (read, write) = pipe2(OFlag::O_CLOEXEC)?;
            write_async(write, content.copy());
            return Ok(read);
        }
        let offer = self.selection.as_ref().ok_or(Error::NotAvailable)?;
        let offered = self
            .offers
            .get(&offer.id())
            .is_some_and(|types| types.iter().any(|t| t == mime_type));
        if !offered {
            return Err(Error::NotAvailable);
        }
        let (read, write) = pipe2(OFlag::O_CLOEXEC)?;
        offer.receive(mime_type.to_string(), write.as_fd());
        self.flush();
        drop(write);
        Ok(read)
    }

    /// Forgets a session. Its selection stays (with what it already
    /// handed over); transfers it never answered fail.
    pub fn close_session(&mut self, session: &ObjectPath<'_>) {
        self.sessions.remove(session);
        if let Some(owned) = self.owned.as_mut()
            && owned.session.as_ref().map(|s| s.as_str()) == Some(session.as_str())
        {
            owned.session = None;
        }
        let unanswered: Vec<u32> = self
            .transfers
            .iter()
            .filter(|(_, t)| t.session.as_str() == session.as_str() && t.done.is_none())
            .map(|(serial, _)| *serial)
            .collect();
        for serial in unanswered {
            if let Some(t) = self.transfers.get_mut(&serial) {
                t.done = Some(false);
            }
            self.finish(serial);
        }
    }

    fn drop_owned(&mut self) {
        if let Some(owned) = self.owned.take() {
            owned.source.destroy();
        }
    }

    fn on_send(&mut self, source: &ExtDataControlSourceV1, mime_type: String, fd: OwnedFd) {
        debug!("send requested: {mime_type}");
        let Some(owned) = self.owned.as_mut().filter(|o| o.source == *source) else {
            return;
        };
        if mime_type == PASSWORD_HINT {
            if owned.sensitive {
                write_async(fd, Secret::new(b"secret".to_vec()));
            }
            return;
        }
        if let Some(content) = owned.cache.get(&mime_type) {
            write_async(fd, content.copy());
            return;
        }
        if let Some(readers) = owned.waiting.get_mut(&mime_type) {
            readers.push(fd);
            return;
        }
        let Some(session) = owned.session.clone() else {
            return;
        };
        if !self
            .sessions
            .get(&session)
            .is_some_and(|s| s.clipboard_enabled)
        {
            return;
        }
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        owned.waiting.insert(mime_type.clone(), vec![fd]);
        self.transfers.insert(
            serial,
            Transfer {
                session: session.clone(),
                mime_type: mime_type.clone(),
                generation: owned.generation,
                writing: false,
                read: None,
                done: None,
            },
        );
        debug!("{session}: asking the app for {mime_type} (transfer {serial})");
        let _ = self.events.send(Event::Transfer {
            session,
            mime_type,
            serial,
        });
    }

    fn on_cancelled(&mut self, source: &ExtDataControlSourceV1) {
        if self.owned.as_ref().is_some_and(|o| o.source == *source) {
            self.owned = None;
        }
        source.destroy();
    }

    fn on_selection(&mut self, offer: Option<ExtDataControlOfferV1>) {
        if let Some(old) = self.selection.take()
            && offer.as_ref() != Some(&old)
        {
            self.offers.remove(&old.id());
            old.destroy();
        }
        let mime_types: Vec<String> = offer
            .as_ref()
            .and_then(|o| self.offers.get(&o.id()))
            .map(|types| {
                types
                    .iter()
                    .filter(|t| *t != PASSWORD_HINT)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let owner = match self.owned.as_mut() {
            Some(o) if o.awaiting_own_offer && offer.is_some() => {
                o.awaiting_own_offer = false;
                o.session.clone()
            }
            _ => None,
        };
        debug!(
            "selection changed: {mime_types:?}, ours: {}",
            owner.is_some()
        );
        self.selection = offer;
        for (path, session) in &self.sessions {
            if session.clipboard_enabled {
                let _ = self.events.send(Event::OwnerChanged {
                    session: path.clone(),
                    mime_types: mime_types.clone(),
                    is_owner: owner.as_ref() == Some(path),
                });
            }
        }
    }
}

fn clean_mime_types(mime_types: Vec<String>) -> Vec<String> {
    let mut clean: Vec<String> = Vec::new();
    for mime in mime_types {
        let valid = !mime.is_empty()
            && mime.len() <= MAX_MIME_LEN
            && mime != PASSWORD_HINT
            && !mime.chars().any(char::is_control);
        if valid && !clean.contains(&mime) {
            clean.push(mime);
        }
        if clean.len() == MAX_MIME_TYPES {
            break;
        }
    }
    clean
}

/// Writes `content` to a reader's pipe without holding anything up.
fn write_async(fd: OwnedFd, content: Secret) {
    thread::spawn(move || {
        let mut file = File::from(fd);
        if let Err(e) = file.write_all(content.as_bytes()) {
            debug!("a clipboard reader went away: {e}");
        }
    });
}

/// Reads the app's content to EOF. Grows the buffer by hand so no
/// reallocation leaves a copy behind unzeroed.
fn read_secret(fd: OwnedFd) -> Result<Secret, ()> {
    let mut file = File::from(fd);
    let mut buf = Secret::new(vec![0u8; 64 * 1024]);
    let mut len = 0;
    loop {
        if len == buf.as_bytes().len() {
            if len >= MAX_CONTENT_BYTES {
                warn!("clipboard content over {MAX_CONTENT_BYTES} bytes; dropped");
                return Err(());
            }
            let mut bigger = vec![0u8; (len * 2).min(MAX_CONTENT_BYTES)];
            bigger[..len].copy_from_slice(buf.as_bytes());
            buf = Secret::new(bigger);
        }
        match file.read(buf.as_mut_bytes(len)) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                warn!("reading the app's clipboard content: {e}");
                return Err(());
            }
        }
    }
    Ok(Secret::new(buf.as_bytes()[..len].to_vec()))
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut clipboard = lock(&state.shared);
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                clipboard.offers.insert(id.id(), Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => clipboard.on_selection(id),
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                if clipboard.selection.as_ref() != Some(&offer) {
                    clipboard.offers.remove(&offer.id());
                    offer.destroy();
                }
            }
            ext_data_control_device_v1::Event::Finished => {
                // The seat is gone; systemd restarts the service.
                error!("the data device was destroyed by the compositor");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            let mut clipboard = lock(&state.shared);
            let types = clipboard.offers.entry(offer.id()).or_default();
            if types.len() < MAX_MIME_TYPES * 2 {
                types.push(mime_type);
            }
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut clipboard = lock(&state.shared);
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                clipboard.on_send(source, mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => clipboard.on_cancelled(source),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_mime_types() {
        let types = vec![
            "text/plain;charset=utf-8".to_string(),
            String::new(),
            PASSWORD_HINT.to_string(),
            "text/plain;charset=utf-8".to_string(),
            "bad\ntype".to_string(),
            "x".repeat(MAX_MIME_LEN + 1),
            "text/plain".to_string(),
        ];
        assert_eq!(
            clean_mime_types(types),
            vec!["text/plain;charset=utf-8", "text/plain"]
        );
    }

    #[test]
    fn caps_the_number_of_types() {
        let types = (0..200).map(|i| format!("type/{i}")).collect();
        assert_eq!(clean_mime_types(types).len(), MAX_MIME_TYPES);
    }

    #[test]
    fn reads_content_across_buffer_growth() {
        let (read, write) = pipe2(OFlag::O_CLOEXEC).unwrap();
        let content: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let expected = content.clone();
        let writer = thread::spawn(move || File::from(write).write_all(&content).unwrap());
        let secret = read_secret(read).unwrap();
        writer.join().unwrap();
        assert_eq!(secret.as_bytes(), &expected[..]);
    }
}
