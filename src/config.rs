use std::path::PathBuf;
use std::{env, fs, io};

use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/xdg/xdg-desktop-portal-omarchy/config.toml";

/// /etc/xdg/xdg-desktop-portal-omarchy/config.toml (or
/// $XDG_DESKTOP_PORTAL_OMARCHY_CONFIG). A missing file means the defaults.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// App ids whose clipboard content is marked as a password
    /// (x-kde-passwordManagerHint: secret), so clipboard histories skip it.
    #[serde(default)]
    pub sensitive_apps: Vec<String>,
}

impl Config {
    pub fn path() -> PathBuf {
        env::var_os("XDG_DESKTOP_PORTAL_OMARCHY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PATH))
    }

    pub fn load() -> io::Result<Self> {
        match fs::read_to_string(Self::path()) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn parse(text: &str) -> io::Result<Self> {
        toml::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn is_sensitive(&self, app_id: &str) -> bool {
        self.sensitive_apps.iter().any(|a| a == app_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sensitive_apps() {
        let config = Config::parse("sensitive_apps = [\"com.bitwarden.desktop\"]\n").unwrap();
        assert!(config.is_sensitive("com.bitwarden.desktop"));
        assert!(!config.is_sensitive("org.mozilla.firefox"));
    }

    #[test]
    fn empty_file_is_the_default() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("sensitive = []\n").is_err());
    }
}
