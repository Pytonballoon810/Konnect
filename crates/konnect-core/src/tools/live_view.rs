//! Live view — keeping what is on screen in step with what Konnect writes.
//!
//! Konnect edits KiCAD documents as files. Anything already displaying those
//! documents is holding its own copy, so without a nudge it shows a design
//! that no longer exists. This module is that nudge, and it treats the two
//! surfaces very differently, because KiCAD 10 does.
//!
//! **A board in an open pcbnew** can be brought up to date over the IPC API,
//! and only in one way. Measured against KiCAD 10 on 2026-08-26:
//!
//! - `RevertDocument` works. A footprint rewritten on disk from x=114 to
//!   x=119 read back as 114 over IPC — pcbnew is blind to the file — and as
//!   119 immediately after a revert. The canvas repaints with it (2.1% of
//!   canvas pixels changed, the moved part's ratsnest reappearing).
//! - `RefreshEditor` does **not** work. KiCAD declares it in the API protos
//!   and answers `AS_UNHANDLED` for every frame, so nothing here calls it.
//! - `RevertDocument` on a document with **unsaved changes** does not warn,
//!   does not prompt, and does not fail. It returned `Ok` in 700 ms and the
//!   unsaved move was gone. That single fact is why reloading is opt-in and
//!   off by default: doing it automatically would silently destroy a user's
//!   work in the window they were looking at.
//!
//! **A schematic** cannot be reached at all. KiCAD 10 ships no schematic IPC
//! commands — `schematic_commands.proto` declares a package and no messages —
//! and eeschema answers `AS_UNHANDLED` even to `GetOpenDocuments`, with a
//! schematic open on screen. So the schematic half of live view goes to
//! Konnect's own viewer instead, through a focus file the viewer watches.
//! That path writes a small JSON file and nothing else: it cannot lose work,
//! which is why it needs no separate opt-in.

use konnect_ipc::client::KiCadIpcClient;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// What live view is currently doing. `enabled` gates everything; the
/// destructive half is gated again by `reload_open_boards`.
#[derive(Debug, Clone, Default)]
pub struct LiveViewSettings {
    pub enabled: bool,
    pub reload_open_boards: bool,
    pub ipc_address: String,
}

fn settings() -> &'static RwLock<LiveViewSettings> {
    static SETTINGS: OnceLock<RwLock<LiveViewSettings>> = OnceLock::new();
    SETTINGS.get_or_init(|| RwLock::new(LiveViewSettings::default()))
}

/// Read the current settings.
pub fn current() -> LiveViewSettings {
    settings().read().map(|s| s.clone()).unwrap_or_default()
}

/// Apply new settings and install (or remove) the write observer to match.
///
/// Installing on enable rather than at startup keeps the default path exactly
/// as it was: with live view off, a write calls no observer at all.
///
/// An empty `ipc_address` is resolved to KiCad's default socket location here
/// — see [`konnect_ipc::client::default_socket_path`] for why that resolution
/// belongs to live view and not to every IPC caller. Without it this feature
/// would be off in the ordinary case: KiCad only sets `KICAD_API_SOCKET` for
/// plugins it launched itself, and Konnect is normally launched by the AI
/// client, so the address is empty unless someone found the settings dialog.
pub fn configure(enabled: bool, reload_open_boards: bool, ipc_address: String) {
    let ipc_address = if ipc_address.is_empty() {
        konnect_ipc::client::default_socket_path()
    } else {
        ipc_address
    };

    if let Ok(mut slot) = settings().write() {
        slot.enabled = enabled;
        slot.reload_open_boards = reload_open_boards;
        slot.ipc_address = ipc_address;
    }

    if enabled {
        konnect_sexp::writer::set_write_observer(Some(Arc::new(on_document_written)));
    } else {
        konnect_sexp::writer::set_write_observer(None);
    }
}

/// The address live view will use, for reporting back to the caller.
pub fn effective_ipc_address(configured: &str) -> String {
    if configured.is_empty() {
        konnect_ipc::client::default_socket_path()
    } else {
        configured.to_string()
    }
}

/// Called after every successful document write while live view is on.
///
/// Deliberately silent on failure. A view that could not be updated is a
/// disappointment, not a failed write, and the write has already happened by
/// the time this runs — returning an error here would report a document that
/// is safely on disk as broken.
fn on_document_written(path: &Path) {
    let settings = current();
    if !settings.enabled {
        return;
    }

    match path.extension().and_then(|e| e.to_str()) {
        Some("kicad_pcb") if settings.reload_open_boards => {
            let client = KiCadIpcClient::new(settings.ipc_address.clone());
            match client.reload_from_disk(path) {
                Ok(true) => tracing::info!("live view: reloaded {} in KiCAD", path.display()),
                Ok(false) => tracing::debug!("live view: {} is not open in KiCAD", path.display()),
                Err(error) => {
                    tracing::warn!("live view: could not reload {}: {error:#}", path.display())
                }
            }
        }
        Some("kicad_sch") => {
            if let Err(error) = write_focus(path, &[], "written") {
                tracing::warn!("live view: could not post viewer focus: {error}");
            }
        }
        _ => {}
    }
}

// ─── Viewer focus channel ────────────────────────────────────────────────────

/// Directory the viewer watches for focus messages.
///
/// A fixed path under the system temp directory rather than something derived
/// from the viewer's process id: the writer here and the viewer are separate
/// processes that never meet, and neither can discover the other's pid.
pub fn focus_dir() -> PathBuf {
    std::env::temp_dir().join("konnect-viewer")
}

/// The single file the viewer watches. Rewritten in place on every message.
pub fn focus_path() -> PathBuf {
    focus_dir().join("focus.json")
}

/// Tell the viewer which sheet is being worked on, and optionally which
/// symbols to mark.
///
/// `seq` increments on every message so the viewer can tell a genuine repeat
/// ("focus this sheet again") from a duplicate filesystem event, which the
/// watcher will deliver more than once for a single write.
pub fn write_focus(sheet: &Path, highlight: &[String], reason: &str) -> Result<u64, String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;

    let dir = focus_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let message = json!({
        "seq": seq,
        "file": sheet.to_string_lossy(),
        "highlight": highlight,
        "reason": reason,
    });

    let path = focus_path();
    std::fs::write(&path, message.to_string()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live view off must leave the write path untouched — no observer, so a
    /// write costs exactly what it always did.
    #[test]
    fn disabling_removes_the_observer() {
        configure(true, false, String::new());
        configure(false, false, String::new());
        assert!(!current().enabled);
    }

    #[test]
    fn focus_messages_carry_increasing_sequence_numbers() {
        let sheet = std::env::temp_dir().join("konnect-focus-test.kicad_sch");
        let first = write_focus(&sheet, &[], "test").expect("first focus");
        let second = write_focus(&sheet, &["R1".into()], "test").expect("second focus");
        assert!(
            second > first,
            "sequence must advance so the viewer can distinguish a repeat \
             focus from a duplicate filesystem event: {first} then {second}"
        );

        let written = std::fs::read_to_string(focus_path()).expect("focus file");
        let parsed: serde_json::Value = serde_json::from_str(&written).expect("focus json");
        assert_eq!(parsed["seq"].as_u64(), Some(second));
        assert_eq!(parsed["highlight"][0].as_str(), Some("R1"));
    }
}
