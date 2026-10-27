//! The backend end to end: a private bus where this test plays the portal
//! frontend, the real compositor ($WAYLAND_DISPLAY, which must offer
//! ext-data-control) and wl-paste as the app that pastes. A fake dialog
//! plugin answers on its own socket.
//!
//! Needs a Wayland session and wl-clipboard, and replaces the clipboard's
//! content, so it's #[ignore]d: `cargo test --test backend -- --ignored`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use futures_util::StreamExt;
use tempfile::TempDir;
use xdg_desktop_portal_omarchy::restore;
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

const BACKEND: &str = "org.freedesktop.impl.portal.desktop.omarchy";
const PATH: &str = "/org/freedesktop/portal/desktop";
const RD: &str = "org.freedesktop.impl.portal.RemoteDesktop";
const CLIPBOARD: &str = "org.freedesktop.impl.portal.Clipboard";
const APP: &str = "org.example.PortalTest";
const TEXT: &str = "text/plain;charset=utf-8";

struct Bus {
    daemon: Child,
    address: String,
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn private_bus() -> Bus {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon");
    let mut address = String::new();
    BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    Bus {
        daemon,
        address: address.trim().to_string(),
    }
}

struct Backend(Child);

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_backend(bus: &Bus, dir: &Path) -> Backend {
    let config = dir.join("config.toml");
    std::fs::write(&config, format!("sensitive_apps = [\"{APP}\"]\n")).unwrap();
    Backend(
        Command::new(env!("CARGO_BIN_EXE_xdg-desktop-portal-omarchy"))
            .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
            .env("XDG_DESKTOP_PORTAL_OMARCHY_CONFIG", &config)
            .env("XDG_DESKTOP_PORTAL_OMARCHY_SOCKET", dir.join("dialog.sock"))
            .spawn()
            .expect("backend"),
    )
}

/// Plays the shell plugin: answers each request with `answer`, and
/// records the requests.
fn fake_dialog(socket: PathBuf, answers: Vec<&'static str>) -> thread::JoinHandle<Vec<String>> {
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        let mut requests = Vec::new();
        for answer in answers {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            requests.push(line.trim().to_string());
            (&stream)
                .write_all(format!("{{\"result\":\"{answer}\"}}\n").as_bytes())
                .unwrap();
        }
        requests
    })
}

async fn frontend(bus: &Bus) -> Connection {
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.portal.Desktop")
        .unwrap()
        .build()
        .await
        .unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    for _ in 0..100 {
        if dbus
            .name_has_owner(BACKEND.try_into().unwrap())
            .await
            .unwrap()
        {
            return conn;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the backend never took its name");
}

type Results = HashMap<String, OwnedValue>;
type Options<'a> = HashMap<&'a str, Value<'a>>;

async fn call<B>(conn: &Connection, iface: &str, method: &str, body: &B) -> zbus::Message
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(Some(BACKEND), PATH, Some(iface), method, body)
        .await
        .unwrap_or_else(|e| panic!("{method}: {e}"))
}

fn path(s: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(s).unwrap()
}

/// CreateSession, SelectDevices (with `restore_data`, if any),
/// RequestClipboard and Start; returns Start's answer.
async fn start_session(
    conn: &Connection,
    id: &str,
    restore_data: Option<OwnedValue>,
) -> (OwnedObjectPath, u32, Results) {
    let session = path(&format!(
        "/org/freedesktop/portal/desktop/session/test/{id}"
    ));
    let request = path(&format!(
        "/org/freedesktop/portal/desktop/request/test/{id}"
    ));
    let reply = call(
        conn,
        RD,
        "CreateSession",
        &(&request, &session, APP, Options::new()),
    )
    .await;
    let (response, _): (u32, Results) = reply.body().deserialize().unwrap();
    assert_eq!(response, 0);

    let mut options = Options::new();
    options.insert("types", Value::from(3u32));
    options.insert("persist_mode", Value::from(2u32));
    if let Some(data) = &restore_data {
        options.insert("restore_data", Value::from(data.try_clone().unwrap()));
    }
    call(
        conn,
        RD,
        "SelectDevices",
        &(&request, &session, APP, options),
    )
    .await;
    call(
        conn,
        CLIPBOARD,
        "RequestClipboard",
        &(&session, Options::new()),
    )
    .await;
    let reply = call(
        conn,
        RD,
        "Start",
        &(&request, &session, APP, "", Options::new()),
    )
    .await;
    let (response, results): (u32, Results) = reply.body().deserialize().unwrap();
    (session, response, results)
}

fn wl_paste(args: &[&str]) -> String {
    let out = Command::new("wl-paste")
        .args(args)
        .output()
        .expect("wl-paste");
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
#[ignore = "needs a Wayland session with ext-data-control and wl-clipboard"]
async fn dialog_grants_and_refuses() {
    let dir = TempDir::new().unwrap();
    let bus = private_bus();
    let dialog = fake_dialog(dir.path().join("dialog.sock"), vec!["deny", "always"]);
    let _backend = start_backend(&bus, dir.path());
    let conn = frontend(&bus).await;

    let (_, response, _) = start_session(&conn, "denied", None).await;
    assert_eq!(response, 1, "a refusal cancels Start");

    let (_, response, results) = start_session(&conn, "always", None).await;
    assert_eq!(response, 0);
    assert!(bool::try_from(&results["clipboard_enabled"]).unwrap());
    assert_eq!(u32::try_from(&results["devices"]).unwrap(), 0);
    assert_eq!(u32::try_from(&results["persist_mode"]).unwrap(), 2);
    assert!(restore::grants_clipboard(&results["restore_data"], APP));

    let requests = dialog.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(&format!("\"app_id\":\"{APP}\"")));
}

#[tokio::test]
#[ignore = "needs a Wayland session with ext-data-control and wl-clipboard"]
async fn no_dialog_means_no() {
    let dir = TempDir::new().unwrap();
    let bus = private_bus();
    let _backend = start_backend(&bus, dir.path());
    let conn = frontend(&bus).await;
    let (_, response, _) = start_session(&conn, "nodialog", None).await;
    assert_eq!(response, 1);
}

#[tokio::test]
#[ignore = "needs a Wayland session with ext-data-control and wl-clipboard"]
async fn only_the_frontend_may_call() {
    let dir = TempDir::new().unwrap();
    let bus = private_bus();
    let _backend = start_backend(&bus, dir.path());
    let _frontend = frontend(&bus).await;
    let other = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let session = path("/org/freedesktop/portal/desktop/session/test/intruder");
    let err = other
        .call_method(
            Some(BACKEND),
            PATH,
            Some(RD),
            "CreateSession",
            &(&session, &session, APP, Options::new()),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("AccessDenied"), "{err}");
}

#[tokio::test]
#[ignore = "needs a Wayland session with ext-data-control and wl-clipboard"]
async fn serves_pastes_from_the_cache_after_one_transfer() {
    let dir = TempDir::new().unwrap();
    let bus = private_bus();
    let _backend = start_backend(&bus, dir.path());
    let conn = frontend(&bus).await;

    // A remembered grant: no dialog.
    let (session, response, _) = start_session(&conn, "paste", Some(restore::encode(APP))).await;
    assert_eq!(response, 0);

    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(CLIPBOARD)
        .unwrap()
        .member("SelectionTransfer")
        .unwrap()
        .build();
    let mut transfers = MessageStream::for_match_rule(rule, &conn, None)
        .await
        .unwrap();

    let mut options = Options::new();
    options.insert("mime_types", Value::from(vec![TEXT]));
    call(&conn, CLIPBOARD, "SetSelection", &(&session, options)).await;

    // The app answers exactly one transfer, as Bitwarden does.
    let message = tokio::time::timeout(Duration::from_secs(5), async {
        let paster = thread::spawn(|| wl_paste(&["--no-newline", "--type", TEXT]));
        let message = transfers.next().await.unwrap().unwrap();
        (message, paster)
    })
    .await
    .expect("a SelectionTransfer");
    let (signal, paster) = message;

    let (signal_session, mime, serial): (OwnedObjectPath, String, u32) =
        signal.body().deserialize().unwrap();
    assert_eq!(signal_session, session);
    assert_eq!(mime, TEXT);

    // Deserializing duplicates the fd: the reply's own copy must go too,
    // or the backend never sees the end of the content.
    let fd: OwnedFd = {
        let reply = call(&conn, CLIPBOARD, "SelectionWrite", &(&session, serial)).await;
        reply.body().deserialize().unwrap()
    };
    let mut pipe = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
    pipe.write_all(b"s3cret").unwrap();
    drop(pipe);
    call(
        &conn,
        CLIPBOARD,
        "SelectionWriteDone",
        &(&session, serial, true),
    )
    .await;
    assert_eq!(paster.join().unwrap(), "s3cret");

    // The session goes away; the content stays, served from the cache.
    let close = conn
        .call_method(
            Some(BACKEND),
            session.as_str(),
            Some("org.freedesktop.impl.portal.Session"),
            "Close",
            &(),
        )
        .await;
    assert!(close.is_ok(), "{close:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let types = wl_paste(&["--list-types"]);
    assert!(types.lines().any(|t| t == TEXT), "{types}");
    assert!(
        types.lines().any(|t| t == "x-kde-passwordManagerHint"),
        "{types}"
    );
    assert_eq!(
        wl_paste(&["--no-newline", "--type", "x-kde-passwordManagerHint"]),
        "secret"
    );
    for _ in 0..2 {
        assert_eq!(wl_paste(&["--no-newline", "--type", TEXT]), "s3cret");
    }
    // No other transfer was asked for.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), transfers.next())
            .await
            .is_err()
    );

    // Leave an innocuous clipboard behind.
    let mut child = Command::new("wl-copy")
        .arg("xdg-desktop-portal-omarchy test")
        .spawn()
        .unwrap();
    child.wait().unwrap();
}
