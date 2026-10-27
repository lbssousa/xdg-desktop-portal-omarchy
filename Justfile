set shell := ["bash", "-uc"]

dev_socket := env("XDG_RUNTIME_DIR", "/run/user/1000") / "xdg-desktop-portal-omarchy-dev.sock"
plugin_link := env("HOME") / ".config/omarchy/plugins/lbssousa.portal"

default:
    @just --list

# Debug build.
build:
    cargo build

# Unit tests, formatting and clippy.
test:
    cargo test --locked
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings

# The backend end to end on a private bus, against this Wayland session
# (replaces the clipboard's content).
test-session:
    cargo test --locked --test backend -- --ignored --test-threads=1

# Run the dialog in a separate Quickshell instance (dev/), listening on its
# own socket, so QML edits only need this restarted — not omarchy-shell.
# `just dev ar` shows it in another language than the shell's.
dev language="":
    #!/usr/bin/env bash
    set -eu
    if [ -n "{{language}}" ]; then
      export OMARCHY_LANGUAGE_FILE=$(mktemp)
      trap 'rm -f "$OMARCHY_LANGUAGE_FILE"' EXIT
      echo "{{language}}" > "$OMARCHY_LANGUAGE_FILE"
    fi
    XDG_DESKTOP_PORTAL_OMARCHY_SOCKET={{dev_socket}} quickshell -p dev

# Ask through the `just dev` instance and print the answer.
try:
    printf '{"v":1,"type":"clipboard","app_id":"com.bitwarden.desktop","app_name":"Bitwarden"}\n' \
      | socat - UNIX-CONNECT:{{dev_socket}}

# Load this checkout's plugin in the real shell (restarts omarchy-shell: a
# keepLoaded service survives plugin hot-reloads).
link:
    ln -sfn "$PWD/plugin" {{plugin_link}}
    omarchy-shell shell enablePlugin lbssousa.portal '{}'
    omarchy-restart-shell

# Build the Arch package into packaging/. --nodeps: cargo usually comes from
# rustup in ~/.cargo, which pacman doesn't know about.
pkg:
    cd packaging && makepkg -f --nodeps
