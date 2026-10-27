//! xdg-desktop-portal backend for Omarchy (Hyprland): the RemoteDesktop and
//! Clipboard portals, for clipboard access only. Sandboxed apps such as the
//! Bitwarden Flatpak can't reach the compositor's data-control protocol, so
//! they put text on the clipboard through the portal (as on GNOME); this
//! backend asks the user in an omarchy-shell dialog and then owns the
//! selection on the app's behalf through ext-data-control.
//!
//! No input devices are ever granted: Start always answers `devices: 0`.

pub mod clipboard;
pub mod config;
pub mod dialog;
pub mod portal;
pub mod restore;
pub mod secret;
