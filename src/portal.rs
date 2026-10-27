//! The D-Bus side: org.freedesktop.impl.portal.RemoteDesktop (clipboard
//! sessions only, never input devices), org.freedesktop.impl.portal.Clipboard,
//! and the Session and Request objects they create. Only the frontend
//! (the owner of org.freedesktop.portal.Desktop) may call them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, ObjectServer, fdo, interface};

use crate::clipboard::{self, Event, Session, Shared};
use crate::config::Config;
use crate::dialog::{self, Answer};
use crate::restore;

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.omarchy";
pub const PATH: &str = "/org/freedesktop/portal/desktop";
const FRONTEND: &str = "org.freedesktop.portal.Desktop";

// Portal responses.
const SUCCESS: u32 = 0;
const CANCELLED: u32 = 1;
const OTHER: u32 = 2;

type Results = HashMap<String, OwnedValue>;

async fn ensure_frontend(conn: &Connection, header: &Header<'_>) -> fdo::Result<()> {
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?;
    let dbus = fdo::DBusProxy::new(conn).await?;
    let owner = dbus
        .get_name_owner(FRONTEND.try_into().expect("valid bus name"))
        .await
        .map_err(|_| fdo::Error::AccessDenied("the portal frontend isn't running".into()))?;
    if owner.as_str() != sender.as_str() {
        return Err(fdo::Error::AccessDenied(
            "only the portal frontend may call this backend".into(),
        ));
    }
    Ok(())
}

fn failed(e: clipboard::Error) -> fdo::Error {
    match e {
        clipboard::Error::Io(e) => fdo::Error::IOError(e.to_string()),
        clipboard::Error::NoSession | clipboard::Error::NoSuchTransfer => {
            fdo::Error::InvalidArgs(e.to_string())
        }
        clipboard::Error::NotEnabled => fdo::Error::AccessDenied(e.to_string()),
        clipboard::Error::NotAvailable => fdo::Error::Failed(e.to_string()),
    }
}

fn value(v: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("plain values hold no file descriptors")
}

pub struct RemoteDesktop {
    pub clipboard: Shared,
    pub config: Arc<Config>,
}

// The method arguments are the portal's own.
#[allow(clippy::too_many_arguments)]
#[interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        ensure_frontend(conn, &header).await?;
        let session = Session {
            app_id,
            ..Session::default()
        };
        clipboard::lock(&self.clipboard)
            .sessions
            .insert(session_handle.clone(), session);
        let object = SessionObject {
            clipboard: self.clipboard.clone(),
            path: session_handle.clone(),
        };
        server.at(&session_handle, object).await?;
        Ok((SUCCESS, Results::new()))
    }

    /// The device types are ignored: none is ever granted. What matters is
    /// the persistence the app asked for, and a restore_data the frontend
    /// may hand back for a token the app presented.
    async fn select_devices(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        ensure_frontend(conn, &header).await?;
        let mut clipboard = clipboard::lock(&self.clipboard);
        let session = clipboard
            .sessions
            .get_mut(&session_handle)
            .filter(|s| s.app_id == app_id)
            .ok_or_else(|| fdo::Error::InvalidArgs("no such session".into()))?;
        session.persist_mode = options
            .get("persist_mode")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0)
            .min(2);
        session.restored = options
            .get("restore_data")
            .is_some_and(|v| restore::grants_clipboard(v, &app_id));
        Ok((SUCCESS, Results::new()))
    }

    async fn start(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        ensure_frontend(conn, &header).await?;
        let (requested, restored, persist_mode) = {
            let clipboard = clipboard::lock(&self.clipboard);
            let session = clipboard
                .sessions
                .get(&session_handle)
                .filter(|s| s.app_id == app_id)
                .ok_or_else(|| fdo::Error::InvalidArgs("no such session".into()))?;
            (
                session.clipboard_requested,
                session.restored,
                session.persist_mode,
            )
        };
        // A session without the clipboard would be a session with nothing.
        if !requested {
            return Ok((OTHER, Results::new()));
        }
        let answer = if restored {
            Answer::Always
        } else {
            ask(server, &handle, &app_id).await
        };
        if answer == Answer::Deny {
            info!("{app_id}: clipboard access refused");
            return Ok((CANCELLED, Results::new()));
        }

        let sensitive = self.config.is_sensitive(&app_id);
        {
            let mut clipboard = clipboard::lock(&self.clipboard);
            let Some(session) = clipboard.sessions.get_mut(&session_handle) else {
                // Closed while the dialog was up.
                return Ok((CANCELLED, Results::new()));
            };
            session.clipboard_enabled = true;
            session.sensitive = sensitive;
        }
        info!(
            "{app_id}: clipboard access granted{}",
            if restored { " (remembered)" } else { "" }
        );

        let mut results = Results::new();
        results.insert("devices".into(), value(0u32));
        results.insert("clipboard_enabled".into(), value(true));
        if answer == Answer::Always && persist_mode > 0 {
            results.insert("persist_mode".into(), value(persist_mode));
            results.insert("restore_data".into(), restore::encode(&app_id));
        }
        Ok((SUCCESS, results))
    }

    #[zbus(property, name = "AvailableDeviceTypes")]
    fn available_device_types(&self) -> u32 {
        0
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

/// Shows the dialog, with a Request object at `handle` so the frontend can
/// close it.
async fn ask(server: &ObjectServer, handle: &OwnedObjectPath, app_id: &str) -> Answer {
    let id = app_id.to_string();
    let pending = match tokio::task::spawn_blocking(move || dialog::Pending::open(&id)).await {
        Ok(Ok(pending)) => pending,
        Ok(Err(e)) => {
            warn!("{app_id}: no clipboard dialog (is omarchy-shell running?): {e}");
            return Answer::Deny;
        }
        Err(_) => return Answer::Deny,
    };
    let canceller = pending.canceller().ok();
    let request = RequestObject {
        canceller: Mutex::new(canceller),
    };
    let exported = server.at(handle, request).await.unwrap_or(false);
    let answer = tokio::task::spawn_blocking(move || pending.wait())
        .await
        .unwrap_or(Answer::Deny);
    if exported {
        let _ = server.remove::<RequestObject, _>(handle).await;
    }
    answer
}

pub struct ClipboardPortal {
    pub clipboard: Shared,
}

#[interface(name = "org.freedesktop.impl.portal.Clipboard")]
impl ClipboardPortal {
    async fn request_clipboard(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        ensure_frontend(conn, &header).await?;
        let mut clipboard = clipboard::lock(&self.clipboard);
        let session = clipboard
            .sessions
            .get_mut(&session_handle)
            .ok_or_else(|| fdo::Error::InvalidArgs("no such session".into()))?;
        session.clipboard_requested = true;
        Ok(())
    }

    async fn set_selection(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        session_handle: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        ensure_frontend(conn, &header).await?;
        let mime_types = options
            .get("mime_types")
            .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default();
        clipboard::lock(&self.clipboard)
            .set_selection(&session_handle, mime_types)
            .map_err(failed)
    }

    async fn selection_write(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        session_handle: OwnedObjectPath,
        serial: u32,
    ) -> fdo::Result<zvariant::OwnedFd> {
        ensure_frontend(conn, &header).await?;
        clipboard::lock(&self.clipboard)
            .selection_write(&self.clipboard, &session_handle, serial)
            .map(zvariant::OwnedFd::from)
            .map_err(failed)
    }

    async fn selection_write_done(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        session_handle: OwnedObjectPath,
        serial: u32,
        success: bool,
    ) -> fdo::Result<()> {
        ensure_frontend(conn, &header).await?;
        clipboard::lock(&self.clipboard)
            .selection_write_done(&session_handle, serial, success)
            .map_err(failed)
    }

    async fn selection_read(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        session_handle: OwnedObjectPath,
        mime_type: String,
    ) -> fdo::Result<zvariant::OwnedFd> {
        ensure_frontend(conn, &header).await?;
        clipboard::lock(&self.clipboard)
            .selection_read(&session_handle, &mime_type)
            .map(zvariant::OwnedFd::from)
            .map_err(failed)
    }

    #[zbus(signal)]
    async fn selection_owner_changed(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn selection_transfer(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

pub struct SessionObject {
    clipboard: Shared,
    path: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        ensure_frontend(conn, &header).await?;
        clipboard::lock(&self.clipboard).close_session(&self.path);
        // Removing the object from inside its own method would wait on
        // itself.
        let conn = conn.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            let _ = conn.object_server().remove::<SessionObject, _>(path).await;
        });
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

pub struct RequestObject {
    canceller: Mutex<Option<dialog::Canceller>>,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    async fn close(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        ensure_frontend(conn, &header).await?;
        if let Some(canceller) = self
            .canceller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            canceller.cancel();
        }
        Ok(())
    }
}

/// Relays what the clipboard has to tell the frontend.
pub async fn emit_events(
    conn: Connection,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(&conn, PATH)?;
    while let Some(event) = events.recv().await {
        let sent = match event {
            Event::Transfer {
                session,
                mime_type,
                serial,
            } => {
                ClipboardPortal::selection_transfer(&emitter, session.as_ref(), &mime_type, serial)
                    .await
            }
            Event::OwnerChanged {
                session,
                mime_types,
                is_owner,
            } => {
                let mut options = HashMap::new();
                options.insert("mime_types", Value::from(mime_types));
                options.insert("session_is_owner", Value::from(is_owner));
                ClipboardPortal::selection_owner_changed(&emitter, session.as_ref(), options).await
            }
        };
        if let Err(e) = sent {
            warn!("emitting a clipboard signal: {e}");
        }
    }
    Ok(())
}
