use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyEvent, KeyEventKind, MouseEvent};
use tokio::sync::mpsc;

use spectatui_core::speckit::registry::{CatalogSource, CatalogTarget};
use spectatui_core::speckit::watch::FsEvent;
use spectatui_core::speckit::{ExtensionInfo, IntegrationInfo, PresetInfo, WorkflowInfo};
use spectatui_core::mux::MuxSession;

#[allow(dead_code)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Tick,
    FsChanged(FsEvent),
    MuxChanged {
        sessions: Vec<String>,
        session: Option<MuxSession>,
        /// Whether the configured backend binary is installed; refreshed by
        /// the poller whenever the backend (or its availability) changes.
        available: bool,
        /// Whether the backend supports ANSI color capture (tmux `capture-pane -e`, herdr
        /// `pane read --ansi`). Equivalent to `available` for the two supported backends.
        color_capable: bool,
    },
    Resize(u16, u16),
    CatalogIndexed {
        /// `false` when the `specify` CLI wasn't runnable, meaning the lists below
        /// degraded to empty rather than reflecting the actual catalogs.
        cli_available: bool,
        integrations: Vec<IntegrationInfo>,
        extensions: Vec<ExtensionInfo>,
        presets: Vec<PresetInfo>,
        workflows: Vec<WorkflowInfo>,
    },
    CatalogSourcesLoaded {
        target: CatalogTarget,
        sources: Vec<CatalogSource>,
    },
    Paste(String),
}

pub struct EventStream {
    rx: mpsc::UnboundedReceiver<AppEvent>,
}

impl EventStream {
    pub fn new(tick_rate: Duration) -> (Self, mpsc::UnboundedSender<AppEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_clone = tx.clone();

        tokio::spawn(async move {
            loop {
                if event::poll(tick_rate).unwrap_or(false) {
                    match event::read() {
                        Ok(CtEvent::Key(key)) => {
                            if key.kind == KeyEventKind::Press
                                && tx_clone.send(AppEvent::Key(key)).is_err()
                            {
                                return;
                            }
                        }
                        Ok(CtEvent::Mouse(mouse)) => {
                            if tx_clone.send(AppEvent::Mouse(mouse)).is_err() {
                                return;
                            }
                        }
                        Ok(CtEvent::Resize(w, h))
                            if tx_clone.send(AppEvent::Resize(w, h)).is_err() =>
                        {
                            return;
                        }
                        Ok(CtEvent::Paste(text)) => {
                            if tx_clone.send(AppEvent::Paste(text)).is_err() {
                                return;
                            }
                        }
                        _ => {}
                    }
                } else if tx_clone.send(AppEvent::Tick).is_err() {
                    return;
                }
            }
        });

        (Self { rx }, tx)
    }

    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }
}
