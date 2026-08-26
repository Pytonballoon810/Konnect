//! Live KiCad GUI IPC regression tests.
//!
//! These tests are ignored by default. The CI live-GUI job launches pcbnew
//! under Xvfb and supplies the socket and a disposable board path.
//! `fixtures/live_ipc.kicad_pcb` is KiCad's GPL-licensed built-in
//! EuroCard160mmX100mm template, used here as a realistic footprint fixture.

use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::{parse_sexp, SexpNode};
use std::path::Path;

fn footprint<'a>(tree: &'a SexpNode, reference: &str) -> &'a SexpNode {
    tree.find_all("footprint")
        .into_iter()
        .find(|node| {
            node.find_all("property").into_iter().any(|property| {
                property.get(1).and_then(SexpNode::as_str) == Some("Reference")
                    && property.get(2).and_then(SexpNode::as_str) == Some(reference)
            })
        })
        .unwrap_or_else(|| panic!("footprint {reference} not found in saved board"))
}

fn at(node: &SexpNode) -> (f64, f64) {
    let at = node.find("at").expect("item has no (at ...) position");
    (
        at.get_f64(1).expect("invalid X coordinate"),
        at.get_f64(2).expect("invalid Y coordinate"),
    )
}

fn footprint_at(node: &SexpNode) -> (f64, f64, f64) {
    let position = node.find("at").expect("footprint has no (at ...) position");
    (
        position.get_f64(1).expect("invalid footprint X"),
        position.get_f64(2).expect("invalid footprint Y"),
        position.get_f64(3).unwrap_or(0.0),
    )
}

fn collect_geometry(node: &SexpNode, output: &mut Vec<(String, f64, f64)>) {
    if matches!(
        node.head(),
        Some("at" | "start" | "mid" | "end" | "center" | "xy")
    ) {
        if let (Some(x), Some(y)) = (node.get_f64(1), node.get_f64(2)) {
            output.push((node.head().unwrap().to_string(), x, y));
        }
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_geometry(child, output);
        }
    }
}

/// Footprint-relative child coordinates, in a canonical order.
///
/// KiCad is free to re-serialize a footprint's graphics in a different order
/// when it rewrites the file — a rotate on a footprint with several silk and
/// courtyard segments reliably shuffles them. The invariant under test is that
/// no child coordinate *changed*, not that KiCad preserved its own ordering,
/// so compare as a sorted multiset.
fn child_geometry(footprint: &SexpNode) -> Vec<(String, f64, f64)> {
    let mut output = Vec::new();
    for child in footprint.children().unwrap_or_default() {
        // The footprint's own position is the only coordinate expected to
        // change. Every nested coordinate is footprint-relative on disk.
        if child.head() != Some("at") {
            collect_geometry(child, &mut output);
        }
    }
    output.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.total_cmp(&b.2))
    });
    output
}

fn pad_offsets(footprint: &SexpNode) -> Vec<(f64, f64)> {
    footprint.find_all("pad").into_iter().map(at).collect()
}

fn load_board(path: &Path) -> SexpNode {
    let source = std::fs::read_to_string(path).expect("failed to read live KiCad board");
    parse_sexp(&source).expect("failed to parse live KiCad board")
}

#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn moving_and_rotating_footprint_preserves_child_geometry() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let reference = std::env::var("KONNECT_LIVE_KICAD_REFERENCE").unwrap_or_else(|_| "MH1".into());
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match client.get_open_documents() {
            Ok(documents) if !documents.is_empty() => break,
            Ok(_) if std::time::Instant::now() < deadline => {}
            Ok(_) => panic!("KiCad has no PCB document open"),
            Err(error)
                if error.to_string().contains("AS_NOT_READY")
                    && std::time::Instant::now() < deadline => {}
            Err(error) => panic!("KiCad IPC connection failed: {error:#}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    client.save_board().expect("initial board save failed");
    let before_tree = load_board(Path::new(&board));
    let before = footprint(&before_tree, &reference);
    let original_at = footprint_at(before);
    let original_pads = pad_offsets(before);
    let original_geometry = child_geometry(before);
    assert!(!original_pads.is_empty(), "test footprint has no pads");

    let target = (original_at.0 + 10.0, original_at.1 + 7.0);
    client
        .move_footprint(&reference, target.0, target.1)
        .expect("footprint move failed");
    client.save_board().expect("moved board save failed");

    let after_tree = load_board(Path::new(&board));
    let after = footprint(&after_tree, &reference);
    let moved_at = at(after);
    assert!((moved_at.0 - target.0).abs() < 1e-6);
    assert!((moved_at.1 - target.1).abs() < 1e-6);
    assert_eq!(
        pad_offsets(after),
        original_pads,
        "moving a footprint must not rewrite its child-relative pad positions"
    );
    assert_eq!(
        child_geometry(after),
        original_geometry,
        "moving a footprint must preserve all child-relative geometry"
    );

    let target_rotation = (original_at.2 + 90.0) % 360.0;
    client
        .rotate_footprint(&reference, target_rotation)
        .expect("footprint rotation failed");
    client.save_board().expect("rotated board save failed");

    let rotated_tree = load_board(Path::new(&board));
    let rotated = footprint(&rotated_tree, &reference);
    assert!((footprint_at(rotated).2 - target_rotation).abs() < 1e-6);
    assert_eq!(
        child_geometry(rotated),
        original_geometry,
        "rotating a footprint must preserve all child-relative geometry"
    );
}

/// #117 regression: v0.2.1 shipped an `add_via` that KiCad rejected outright
/// with `AS_BAD_REQUEST "could not unpack PCB_VIA"`, because the padstack
/// carried two copper entries under PST_NORMAL.
///
/// Nothing offline can catch that class: the message is schema-valid, so it
/// encodes and decodes cleanly — only KiCad's own `Deserialize` refuses it.
/// This test is the gate; run it (and the rest of this file) before tagging a
/// release, not just weekly.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn adding_a_via_actually_creates_it_on_the_board() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);

    let net = client
        .get_nets()
        .expect("net list query failed")
        .into_iter()
        .find(|net| !net.name.is_empty())
        .expect("board has no named net to attach a via to");

    client.save_board().expect("initial board save failed");
    let vias_before = load_board(Path::new(&board)).find_all("via").len();

    // Somewhere clear of the EuroCard template's own content.
    let (x, y) = (40.0, 40.0);
    client
        .add_via(&net.name, x, y, 0.4, 0.8)
        .expect("add_via reported an error");
    client
        .save_board()
        .expect("board save after add_via failed");

    let after = load_board(Path::new(&board));
    let vias: Vec<_> = after.find_all("via");
    assert_eq!(
        vias.len(),
        vias_before + 1,
        "add_via returned Ok but the saved board has no new via — this is \
         exactly the v0.2.1 failure mode (silent success, nothing created)"
    );
    let placed = vias
        .iter()
        .find(|via| {
            via.find("at")
                .map(|node| {
                    (node.get_f64(1).unwrap_or_default() - x).abs() < 1e-6
                        && (node.get_f64(2).unwrap_or_default() - y).abs() < 1e-6
                })
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no via at ({x}, {y}) in the saved board"));
    assert!(
        placed.find("size").is_some() && placed.find("drill").is_some(),
        "via is missing its size/drill: {placed:?}"
    );
}

/// An open pcbnew is blind to a board rewritten underneath it, and
/// `RevertDocument` is what makes it look again.
///
/// This is the whole basis of live view for boards, and every part of it was
/// measured rather than assumed (KiCad 10, 2026-08-26):
///
/// - pcbnew keeps its own copy. A footprint moved on disk still reads at its
///   old position over IPC — asserted below, because a future KiCad that
///   started watching files would make the revert unnecessary and this test
///   is where that would show up.
/// - `RevertDocument` replaces that copy with the file's contents.
/// - It does so **even with unsaved changes**, without prompting, in about
///   700 ms. The second half of this test pins that down: it is the reason
///   reverting is opt-in, and a KiCad that started prompting would hang a
///   tool call on a modal instead.
///
/// `RefreshEditor` is deliberately absent: KiCad 10 answers `AS_UNHANDLED`
/// for every frame, and the revert repaints the canvas on its own.
#[test]
#[ignore = "requires a running KiCad GUI with its IPC API enabled"]
fn reverting_makes_pcbnew_show_the_file_on_disk() {
    let board = std::env::var("KONNECT_LIVE_KICAD_BOARD")
        .expect("KONNECT_LIVE_KICAD_BOARD must name the disposable open board");
    let socket = std::env::var("KICAD_API_SOCKET").expect("KICAD_API_SOCKET is required");
    let client = KiCadIpcClient::new(socket);
    let path = Path::new(&board);

    client.save_board().expect("initial board save failed");
    let reference = client
        .list_footprints()
        .expect("list_footprints")
        .into_iter()
        .find(|f| !f.reference.is_empty())
        .map(|f| f.reference)
        .expect("board has no referenced footprint");

    let before = client
        .get_footprint(&reference)
        .expect("get_footprint")
        .expect("footprint vanished");

    // Rewrite the file the way an external tool does — KiCad is not told.
    let source = std::fs::read_to_string(path).expect("read board");
    let shifted = shift_footprint_y(&source, &reference, 5.0)
        .unwrap_or_else(|| panic!("could not shift {reference} in the board file"));
    std::fs::write(path, &shifted).expect("write board");

    let unaware = client
        .get_footprint(&reference)
        .expect("get_footprint")
        .expect("footprint vanished");
    assert_eq!(
        unaware.position.y, before.position.y,
        "pcbnew reported the on-disk position without being told to reload — \
         if KiCad has started following the file, live view's revert is no \
         longer needed and its unsaved-work hazard can go with it"
    );

    assert!(
        client.reload_from_disk(path).expect("reload_from_disk"),
        "the board under test must be the one KiCad has open"
    );

    let reloaded = client
        .get_footprint(&reference)
        .expect("get_footprint")
        .expect("footprint vanished");
    assert!(
        (reloaded.position.y - (before.position.y + 5.0)).abs() < 1e-6,
        "after reverting, pcbnew should show the file's {} — it shows {}",
        before.position.y + 5.0,
        reloaded.position.y
    );

    // An unsaved change is discarded silently. Timed, because a KiCad that
    // began asking would block the call on a dialog rather than fail it.
    client
        .move_footprint(&reference, reloaded.position.x, reloaded.position.y + 12.0)
        .expect("move_footprint");
    let started = std::time::Instant::now();
    let document = client.find_open_board(path).expect("find_open_board");
    client.revert_document(document).expect("revert");
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "reverting a modified document took {elapsed:?} — KiCad may now be \
         prompting, which would hang a tool call"
    );
    let after = client
        .get_footprint(&reference)
        .expect("get_footprint")
        .expect("footprint vanished");
    assert!(
        (after.position.y - reloaded.position.y).abs() < 1e-6,
        "the unsaved move survived a revert — if KiCad has started protecting \
         unsaved work, set_live_view's warning is now wrong"
    );
}

/// Shift the `(at x y [angle])` of the footprint carrying `reference` by `dy`
/// millimetres, editing the s-expression text the way an external tool would.
fn shift_footprint_y(text: &str, reference: &str, dy: f64) -> Option<String> {
    let anchor = text.find(&format!("\"Reference\" \"{reference}\""))?;
    // A footprint's own (at ...) precedes its properties.
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
