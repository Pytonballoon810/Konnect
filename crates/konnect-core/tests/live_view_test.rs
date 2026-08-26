//! Live view, end to end, against a running KiCAD.
//!
//! The pieces are covered separately elsewhere — `konnect-sexp` proves every
//! write path calls the observer, `konnect-ipc` proves `RevertDocument` makes
//! pcbnew show the file. This asserts the join: that turning live view on is
//! enough for an ordinary document write to reach the screen, with nobody in
//! between remembering to ask.
//!
//! Ignored by default; needs KiCAD open on the board named by
//! `KONNECT_LIVE_KICAD_BOARD`, with its IPC API enabled.

use konnect_core::tools::live_view;
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{read_consistent, write_atomic};
use std::path::Path;

/// Shift the `(at x y [angle])` of the footprint carrying `reference` by `dy`
/// millimetres — an edit made as a file, the way Konnect's tools make them.
fn shift_footprint_y(text: &str, reference: &str, dy: f64) -> Option<String> {
    let anchor = text.find(&format!("\"Reference\" \"{reference}\""))?;
    let start = text[..anchor].rfind("(footprint ")?;
    let at = text[start..anchor].find("(at ")? + start;
    let end = text[at..].find(')')? + at;
    let mut parts = text[at + 4..end].split_whitespace();
    let x: f64 = parts.next()?.parse().ok()?;
    let y: f64 = parts.next()?.parse().ok()?;
    let rest: Vec<&str> = parts.collect();
    let tail = if rest.is_empty() {
        String::new()
    } else {
        format!(" {}", rest.join(" "))
    };
    Some(format!(
        "{}(at {} {}{}{}",
        &text[..at],
        x,
        y + dy,
        tail,
        &text[end..]
    ))
}

#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn a_board_write_reaches_an_open_pcbnew_once_live_view_is_on() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let path = Path::new(&board);

    // Deliberately left unset unless the environment supplies one, so this
    // also exercises live view resolving KiCad's default socket by itself —
    // the ordinary case, since Konnect is normally launched by an AI client
    // and inherits no KICAD_API_SOCKET.
    let configured = std::env::var("KICAD_API_SOCKET").unwrap_or_default();
    let socket = live_view::effective_ipc_address(&configured);
    let client = KiCadIpcClient::new(socket.clone());

    let reference = client
        .list_footprints()
        .expect("KiCAD must be running with the board open")
        .into_iter()
        .find(|f| !f.reference.is_empty())
        .map(|f| f.reference)
        .expect("board has no referenced footprint");
    let before = client
        .get_footprint(&reference)
        .expect("get_footprint")
        .expect("footprint vanished");

    // Off by default: the same write must change nothing on screen.
    live_view::configure(false, false, socket.clone());
    let source = read_consistent(path).expect("read board");
    let shifted = shift_footprint_y(&source, &reference, 3.0).expect("rewrite");
    write_atomic(path, &shifted).expect("write board");

    let unchanged = client
        .get_footprint(&reference)
        .expect("get_footprint")
        .expect("footprint vanished");
    assert_eq!(
        unchanged.position.y, before.position.y,
        "with live view off, a write must not touch KiCAD — reaching into the \
         user's editor is exactly what they did not ask for"
    );

    // On, including boards: the next write should land on screen by itself.
    live_view::configure(true, true, socket);
    let source = read_consistent(path).expect("read board");
    let shifted = shift_footprint_y(&source, &reference, 4.0).expect("rewrite");
    write_atomic(path, &shifted).expect("write board");

    let live = client
        .get_footprint(&reference)
        .expect("get_footprint")
        .expect("footprint vanished");
    assert!(
        (live.position.y - (before.position.y + 7.0)).abs() < 1e-6,
        "pcbnew should show the file's {} after a live-view write — it shows \
         {}. Both edits are on disk; only the second should have been pushed",
        before.position.y + 7.0,
        live.position.y
    );

    live_view::configure(false, false, String::new());
}
