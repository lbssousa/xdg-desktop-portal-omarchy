use std::process::ExitCode;
use std::sync::Arc;

use tracing::level_filters::LevelFilter;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use xdg_desktop_portal_omarchy::config::Config;
use xdg_desktop_portal_omarchy::portal::{self, BUS_NAME, ClipboardPortal, PATH, RemoteDesktop};
use xdg_desktop_portal_omarchy::{clipboard, secret};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // journald stamps the lines; RUST_LOG=debug for more.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_target(false)
        .without_time()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
    secret::harden();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = Config::load().map_err(|e| format!("{}: {e}", Config::path().display()))?;
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (shared, wayland) = clipboard::connect(events_tx)?;

    std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || {
            if let Err(e) = wayland.run() {
                error!("Wayland connection lost: {e}");
            }
            // systemd (Restart=on-failure) brings it back with the session.
            std::process::exit(1);
        })?;

    let conn = zbus::connection::Builder::session()?
        .serve_at(
            PATH,
            RemoteDesktop {
                clipboard: shared.clone(),
                config: Arc::new(config),
            },
        )?
        .serve_at(PATH, ClipboardPortal { clipboard: shared })?
        .name(BUS_NAME)?
        .build()
        .await?;
    info!("serving {BUS_NAME}");

    portal::emit_events(conn, events_rx).await?;
    Ok(())
}
