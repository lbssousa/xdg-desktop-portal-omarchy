//! Asks the user, through the omarchy-shell plugin (lbssousa.portal), whether
//! an app may use the clipboard. One NDJSON request per connection on
//! $XDG_RUNTIME_DIR/xdg-desktop-portal-omarchy.sock:
//!
//!   -> {"v":1,"type":"clipboard","app_id":"…","app_name":"…","app_names":{"pt_BR":"…"}}
//!   <- {"result":"allow"|"always"|"deny"|"busy"|"error", "message"?: "…"}
//!
//! Closing the connection before the answer closes the dialog. Anything but
//! "allow"/"always" — including no shell, a socket of another user or a
//! malformed answer — is a refusal. `app_names` holds the desktop entry's
//! localized names by locale (`Name[pt_BR]=`), for the dialog to pick the
//! one of the shell's language.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::getuid;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_RESPONSE_BYTES: u64 = 4096;
// The dialog has no timeout of its own; nobody answering means no.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_NAME_CHARS: usize = 80;
const MAX_LOCALIZED_NAMES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// This session only.
    Allow,
    /// This session, and later ones that present its restore token.
    Always,
    Deny,
}

#[derive(Serialize)]
struct Request<'a> {
    v: u32,
    #[serde(rename = "type")]
    kind: &'a str,
    app_id: &'a str,
    app_name: &'a str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    app_names: &'a BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Response {
    result: String,
}

pub fn socket_path() -> PathBuf {
    if let Some(p) = env::var_os("XDG_DESKTOP_PORTAL_OMARCHY_SOCKET") {
        return PathBuf::from(p);
    }
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", getuid())));
    runtime.join("xdg-desktop-portal-omarchy.sock")
}

/// Connects and makes sure the listening end belongs to this user.
pub fn connect() -> io::Result<UnixStream> {
    let stream = UnixStream::connect(socket_path())?;
    let creds = getsockopt(&stream, PeerCredentials).map_err(io::Error::from)?;
    if creds.uid() != getuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "dialog socket is owned by another user",
        ));
    }
    Ok(stream)
}

/// A dialog in progress. `canceller` shuts its connection down from
/// another thread (the portal request was closed), which makes `wait`
/// return Deny and the plugin close the dialog.
pub struct Pending {
    stream: UnixStream,
}

impl Pending {
    pub fn open(app_id: &str) -> io::Result<Self> {
        let mut stream = connect()?;
        stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
        let names = app_names(app_id).unwrap_or_default();
        let request = Request {
            v: PROTOCOL_VERSION,
            kind: "clipboard",
            app_id,
            app_name: names.name.as_deref().unwrap_or(app_id),
            app_names: &names.localized,
        };
        let mut line = serde_json::to_vec(&request).map_err(io::Error::other)?;
        line.push(b'\n');
        stream.write_all(&line)?;
        Ok(Self { stream })
    }

    pub fn canceller(&self) -> io::Result<Canceller> {
        Ok(Canceller(self.stream.try_clone()?))
    }

    /// Blocks until the user answers.
    pub fn wait(self) -> Answer {
        let mut line = String::new();
        let mut reader = BufReader::new(io::Read::take(&self.stream, MAX_RESPONSE_BYTES));
        if reader.read_line(&mut line).is_err() {
            return Answer::Deny;
        }
        parse_answer(&line)
    }
}

pub struct Canceller(UnixStream);

impl Canceller {
    pub fn cancel(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

pub fn parse_answer(line: &str) -> Answer {
    match serde_json::from_str::<Response>(line.trim_end()) {
        Ok(r) if r.result == "allow" => Answer::Allow,
        Ok(r) if r.result == "always" => Answer::Always,
        _ => Answer::Deny,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AppNames {
    /// Name=
    pub name: Option<String>,
    /// Name[locale]=, by locale as the entry writes it (pt_BR, sr@latin).
    pub localized: BTreeMap<String, String>,
}

/// The names in the app's desktop entry, looked up in $XDG_DATA_HOME and
/// $XDG_DATA_DIRS (where Flatpak exports its apps' entries). Only labels
/// for the dialog, next to the app id it always shows.
pub fn app_names(app_id: &str) -> Option<AppNames> {
    if app_id.is_empty() || app_id.contains('/') || app_id.starts_with('.') {
        return None;
    }
    let home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")));
    let dirs =
        env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    home.into_iter()
        .chain(env::split_paths(&dirs))
        .map(|dir| dir.join("applications").join(format!("{app_id}.desktop")))
        .find_map(|path| fs::read_to_string(path).ok())
        .map(|entry| desktop_entry_names(&entry))
}

fn desktop_entry_names(entry: &str) -> AppNames {
    let mut names = AppNames::default();
    let mut in_main = false;
    for line in entry.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_main) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let value: String = value.chars().take(MAX_NAME_CHARS).collect();
        let key = key.trim_end();
        if key == "Name" {
            names.name.get_or_insert(value);
        } else if let Some(locale) = key.strip_prefix("Name[").and_then(|k| k.strip_suffix(']'))
            && is_locale(locale)
            && names.localized.len() < MAX_LOCALIZED_NAMES
        {
            names.localized.entry(locale.to_string()).or_insert(value);
        }
    }
    names
}

// lang[_COUNTRY][@modifier], as desktop entries write locales.
fn is_locale(locale: &str) -> bool {
    (2..=24).contains(&locale.len())
        && locale
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(parse_answer("{\"result\":\"allow\"}\n"), Answer::Allow);
        assert_eq!(parse_answer("{\"result\":\"always\"}"), Answer::Always);
        assert_eq!(parse_answer("{\"result\":\"deny\"}"), Answer::Deny);
        assert_eq!(parse_answer("{\"result\":\"busy\"}"), Answer::Deny);
        assert_eq!(parse_answer("garbage"), Answer::Deny);
        assert_eq!(parse_answer(""), Answer::Deny);
    }

    #[test]
    fn reads_the_main_names_only() {
        let entry = "[Desktop Action new]\nName=New window\nName[de]=Neues Fenster\n[Desktop Entry]\nName[pt_BR]=Cofre\nName[sr@latin]=Trezor\nName[../x]=Bad\nName=Bitwarden\n";
        let names = desktop_entry_names(entry);
        assert_eq!(names.name.as_deref(), Some("Bitwarden"));
        assert_eq!(
            names.localized,
            BTreeMap::from([
                ("pt_BR".to_string(), "Cofre".to_string()),
                ("sr@latin".to_string(), "Trezor".to_string()),
            ])
        );
        assert_eq!(
            desktop_entry_names("[Desktop Entry]\nExec=x\n"),
            AppNames::default()
        );
    }

    #[test]
    fn ignores_path_like_app_ids() {
        assert_eq!(app_names("../../etc/passwd"), None);
        assert_eq!(app_names(""), None);
    }

    #[test]
    fn request_shape() {
        let request = Request {
            v: 1,
            kind: "clipboard",
            app_id: "com.bitwarden.desktop",
            app_name: "Bitwarden",
            app_names: &BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"v":1,"type":"clipboard","app_id":"com.bitwarden.desktop","app_name":"Bitwarden"}"#
        );
        let localized = BTreeMap::from([("pt_BR".to_string(), "Cofre".to_string())]);
        let request = Request {
            app_names: &localized,
            ..request
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"v":1,"type":"clipboard","app_id":"com.bitwarden.desktop","app_name":"Bitwarden","app_names":{"pt_BR":"Cofre"}}"#
        );
    }
}
