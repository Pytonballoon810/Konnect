//! Update a board's nets from its schematic — the programmatic half of
//! eeschema's *Update PCB from Schematic* (F8).
//!
//! # Why this exists
//!
//! Without it the only way to reconcile a board with its schematic is the GUI,
//! which means a human has to be in the loop for something that is pure data
//! transformation. Worse, the failure is silent: a board whose net table has
//! drifted still opens, still passes a courtyard check, and still looks right —
//! it just has pads on nets that no longer exist, so anything routed from it is
//! wired to nothing.
//!
//! The specific drift this was written for: a footprint swap can blank a pad's
//! net, and a later partial sync can then *add* the schematic's net alongside
//! the stale one, leaving two entries with disjoint pads — `IF2_TX` holding the
//! MCU pin and `/IF2_TX` holding the connector, with nothing joining them.
//!
//! # What it does, and what it deliberately does not
//!
//! Does:
//!   * reassigns every pad's net from `(reference, pad)` → net name
//!   * **drops** a pad's net when the schematic gives it none, which is the
//!     part that actually clears staleness — a merge cannot, because a merge
//!     has no way to know an old entry is no longer wanted
//!
//! Does not: add, delete or move footprints. Those need library resolution and
//! placement decisions, and getting them wrong damages a layout rather than
//! merely leaving it stale. Mismatches are reported for a human to act on.
//!
//! # Method
//!
//! `SexpNode` carries no byte offsets, so the house pattern applies: parse for
//! semantics, scan the text for edit spans. The scanner tracks paren depth and
//! skips quoted strings, so it is indifferent to whether the file was written
//! by KiCad 9 (two spaces) or KiCad 10 (tabs) — the same reason
//! `pcb_board::close_of_block` exists.
//!
//! # KiCad 10 dropped the net table
//!
//! In file version 20260206 a pad's net is `(net "/IF2_TX")` — the name alone.
//! There is no numeric id and no top-level `(net N "name")` declaration block;
//! a 69-footprint board contains 386 `(net …)` nodes and every one of them is a
//! pad child. KiCad 9 wrote `(net 45 "/IF2_TX")` against a table, and code
//! written for that shape silently does nothing useful on a v10 file, because
//! the table it wants to rebuild is not there.
//!
//! This targets v10 and **refuses** a file carrying the legacy id form rather
//! than half-converting it. Renumbering nets wrongly is worse than not running:
//! it produces a board that opens cleanly and is wired incorrectly.

use std::collections::{BTreeMap, BTreeSet};

use konnect_sexp::{apply_edits, parse_sexp, write_atomic, SexpEdit};
use serde_json::json;

use super::{get_path, CallToolResult, ToolContext};

// ─── Netlist ─────────────────────────────────────────────────────────────────

/// What the schematic says: net name → the `(reference, pad)` pairs on it.
#[derive(Debug, Default)]
pub struct Netlist {
    /// Net name in netlist order. Order is kept because it is stable across
    /// exports, which keeps the diff of a re-sync small.
    pub nets: Vec<String>,
    /// `(reference, pad)` → net name.
    pub pad_nets: BTreeMap<(String, String), String>,
    /// Every reference the schematic knows about.
    pub refs: BTreeSet<String>,
}

/// Parse a `kicadsexpr` netlist as produced by `kicad-cli sch export netlist`.
pub fn parse_netlist(source: &str) -> anyhow::Result<Netlist> {
    let tree = parse_sexp(source)?;
    let mut out = Netlist::default();

    if let Some(comps) = tree.find("components") {
        for comp in comps.find_all("comp") {
            if let Some(r) = comp.find_str("ref") {
                out.refs.insert(r.to_string());
            }
        }
    }

    let nets = tree
        .find("nets")
        .ok_or_else(|| anyhow::anyhow!("netlist has no (nets) section"))?;

    for net in nets.find_all("net") {
        // (net (code "3") (name "/IF2_TX") (node (ref "U1") (pin "13") ...) ...)
        let Some(name) = net.find_str("name") else {
            continue;
        };
        // The unconnected net is not a real net and must not enter the table;
        // KiCad reserves id 0 for it and pads simply omit the child.
        if name.is_empty() {
            continue;
        }
        if !out.nets.iter().any(|n| n == name) {
            out.nets.push(name.to_string());
        }
        for node in net.find_all("node") {
            let (Some(r), Some(p)) = (node.find_str("ref"), node.find_str("pin")) else {
                continue;
            };
            out.pad_nets
                .insert((r.to_string(), p.to_string()), name.to_string());
        }
    }

    Ok(out)
}

// ─── Board scanning ──────────────────────────────────────────────────────────

/// A `(tag …)` node located in the source text.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

/// Byte offset of the `)` closing the block that opens at `open`, or `None` if
/// the file is unbalanced. Quoted strings — which routinely contain parens in
/// descriptions and datasheet URLs — are skipped.
fn close_of(content: &str, open: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else {
            match c {
                '"' => in_str = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Every `(tag …)` node opening directly at `depth` inside `range`.
///
/// Depth is counted from the start of `range`, where the enclosing block's own
/// `(` has not yet been seen — so a top-level `(net …)` in a `.kicad_pcb`,
/// which sits inside `(kicad_pcb …)`, is at depth 1.
fn nodes_at_depth(content: &str, range: (usize, usize), tag: &str, depth: usize) -> Vec<Span> {
    let bytes = content.as_bytes();
    let mut found = Vec::new();
    let mut cur = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = range.0;
    while i < range.1 {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' => {
                cur += 1;
                if cur == depth + 1 {
                    let rest = &content[i + 1..];
                    let head_len = rest
                        .find(|ch: char| ch.is_whitespace() || ch == '(' || ch == ')')
                        .unwrap_or(0);
                    if &rest[..head_len] == tag {
                        if let Some(end) = close_of(content, i) {
                            found.push(Span { start: i, end });
                        }
                    }
                }
            }
            ')' => cur = cur.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    found
}

/// The `Reference` property of a footprint block.
fn footprint_reference(content: &str, fp: Span) -> Option<String> {
    let block = &content[fp.start..=fp.end];
    let marker = r#"(property "Reference" ""#;
    let at = block.find(marker)? + marker.len();
    let end = block[at..].find('"')? + at;
    Some(block[at..end].to_string())
}

/// A pad's number, i.e. the first quoted token after `(pad`.
fn pad_number(content: &str, pad: Span) -> Option<String> {
    let block = &content[pad.start..=pad.end];
    let q = block.find('"')? + 1;
    let end = block[q..].find('"')? + q;
    Some(block[q..end].to_string())
}

/// The name inside a `(net "…")` node.
fn net_name(content: &str, net: Span) -> Option<String> {
    let block = &content[net.start..=net.end];
    let q = block.find('"')? + 1;
    let end = block[q..].find('"')? + q;
    Some(block[q..end].to_string())
}

/// Whether the file uses KiCad 9's `(net <id> "name")` form anywhere.
///
/// Detected rather than converted. The id form is meaningless without the
/// top-level table that gives the ids meaning, and rewriting one without the
/// other produces a board that opens cleanly and is wired wrongly — the single
/// worst outcome available here.
fn has_legacy_net_ids(content: &str) -> bool {
    content.match_indices("(net ").any(|(i, _)| {
        content[i + 5..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

/// The leading whitespace of the line `offset` sits on.
fn indent_at(content: &str, offset: usize) -> &str {
    let line_start = content[..offset].rfind('\n').map(|p| p + 1).unwrap_or(0);
    &content[line_start..offset]
}

/// The pad children KiCad's writer emits *before* `(net …)`.
///
/// From `pcb_io_kicad_sexpr`'s fixed field order: geometry, then layers, then
/// the shape modifiers, then the net, then `pinfunction`/`pintype`/`uuid`.
/// Listing the leaders rather than the followers is the safer half — a KiCad
/// version that adds a new trailing field still lands the net in the right
/// place, whereas an unknown *leading* field would push it too early.
const PAD_CHILDREN_BEFORE_NET: &[&str] = &[
    "at",
    "size",
    "drill",
    "property",
    "layers",
    "roundrect_rratio",
    "chamfer_ratio",
    "chamfer",
    "rect_delta",
];

/// Every direct child of `block`, as `(tag, span)` in file order.
fn children_of(content: &str, block: Span) -> Vec<(&str, Span)> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = block.start;
    while i <= block.end {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' => {
                depth += 1;
                if depth == 2 {
                    let rest = &content[i + 1..];
                    let n = rest
                        .find(|ch: char| ch.is_whitespace() || ch == '(' || ch == ')')
                        .unwrap_or(0);
                    if let Some(end) = close_of(content, i) {
                        out.push((&rest[..n], Span { start: i, end }));
                    }
                }
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    out
}

/// Where a missing `(net …)` child belongs inside `pad`, as `(offset, indent)`.
///
/// KiCad's field order is fixed, so putting the child back where KiCad would
/// have written it makes a repair byte-identical to what F8 produces. Inserting
/// it as the first child instead parses identically and is not wrong — it just
/// makes every subsequent diff of the file noisy for no reason.
fn net_insert_point(content: &str, pad: Span) -> (usize, String) {
    let kids = children_of(content, pad);
    let anchor = kids
        .iter()
        .rfind(|(tag, _)| PAD_CHILDREN_BEFORE_NET.contains(tag));

    match anchor {
        Some((_, span)) => (span.end + 1, indent_at(content, span.start).to_string()),
        None => {
            // No recognised leader: fall back to first-child position, taking
            // the indent from whatever child does exist.
            let after = content[pad.start..]
                .find('\n')
                .map(|p| pad.start + p)
                .unwrap_or(pad.start);
            let indent = kids
                .first()
                .map(|(_, s)| indent_at(content, s.start).to_string())
                .unwrap_or_else(|| format!("{}\t", indent_at(content, pad.start)));
            (after, indent)
        }
    }
}

// ─── The update ──────────────────────────────────────────────────────────────

/// Outcome of a sync, reported whether or not it was applied.
#[derive(Debug, Default, serde::Serialize)]
pub struct SyncReport {
    pub nets_in_schematic: usize,
    /// Distinct net names the board's pads carried before the sync. A board in
    /// step has the same count as the schematic; more means split or stale
    /// names, which is the drift being cleared.
    pub nets_on_board_before: usize,
    pub pads_assigned: usize,
    pub pads_cleared: usize,
    pub pads_unchanged: usize,
    /// On the board but absent from the schematic — F8 would offer to delete
    /// these. Reported only.
    pub extra_footprints: Vec<String>,
    /// In the schematic but absent from the board — F8 would place these.
    /// Reported only.
    pub missing_footprints: Vec<String>,
    /// Pads carrying a net the schematic does not mention. Non-empty here is
    /// exactly the staleness this tool exists to clear, so it is listed rather
    /// than counted.
    pub stale_nets_removed: Vec<String>,
}

/// Rewrite `board`'s pad net assignments from `netlist`.
///
/// Returns the new file content and a report. Pure: does no I/O, so the
/// interesting logic is testable without a KiCad install.
pub fn reconcile(content: &str, nl: &Netlist) -> anyhow::Result<(String, SyncReport)> {
    if has_legacy_net_ids(content) {
        anyhow::bail!(
            "this board uses KiCad 9's numeric net form `(net <id> \"name\")`; \
             update_pcb_from_schematic targets the KiCad 10 name-only form and \
             will not half-convert it. Open the board in KiCad 10 once to \
             migrate it, then re-run."
        );
    }

    let mut report = SyncReport {
        nets_in_schematic: nl.nets.len(),
        ..Default::default()
    };
    let whole = (0usize, content.len());

    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut seen_refs: BTreeSet<String> = BTreeSet::new();
    let mut stale: BTreeSet<String> = BTreeSet::new();
    let mut before: BTreeSet<String> = BTreeSet::new();

    for fp in nodes_at_depth(content, whole, "footprint", 1) {
        let Some(reference) = footprint_reference(content, fp) else {
            continue;
        };
        seen_refs.insert(reference.clone());

        for pad in nodes_at_depth(content, (fp.start, fp.end), "pad", 1) {
            let Some(num) = pad_number(content, pad) else {
                continue;
            };
            let want = nl.pad_nets.get(&(reference.clone(), num.clone()));
            let have = nodes_at_depth(content, (pad.start, pad.end), "net", 1);
            let current = have.first().and_then(|s| net_name(content, *s));
            if let Some(c) = &current {
                before.insert(c.clone());
            }

            match (want, have.first()) {
                (Some(name), Some(span)) => {
                    let new = format!("(net \"{name}\")");
                    if content[span.start..=span.end] == new {
                        report.pads_unchanged += 1;
                    } else {
                        if let Some(c) = &current {
                            if c != name {
                                stale.insert(c.clone());
                            }
                        }
                        edits.push(SexpEdit::replace(span.start, span.end + 1, new));
                        report.pads_assigned += 1;
                    }
                }
                (Some(name), None) => {
                    // Pad has no net child yet — put one where KiCad's own
                    // writer would have, so a repair is byte-identical to F8.
                    let (at, indent) = net_insert_point(content, pad);
                    edits.push(SexpEdit::insert(at, format!("\n{indent}(net \"{name}\")")));
                    report.pads_assigned += 1;
                }
                (None, Some(span)) => {
                    // The schematic gives this pad no net. Removing the child is
                    // what a merge cannot do and what clears the drift. KiCad 10
                    // writes no `(net "")` for an unconnected pad, it omits the
                    // child entirely — 484 pads and 386 net children on the board
                    // this was checked against.
                    if let Some(c) = current {
                        if !c.is_empty() {
                            stale.insert(c);
                        }
                    }
                    // Take the whole line, newline included, so no blank line is
                    // left where the child was.
                    let line_start = content[..span.start].rfind('\n').unwrap_or(span.start);
                    edits.push(SexpEdit::delete(line_start, span.end + 1));
                    report.pads_cleared += 1;
                }
                (None, None) => report.pads_unchanged += 1,
            }
        }
    }

    report.nets_on_board_before = before.len();
    report.stale_nets_removed = stale.into_iter().collect();
    report.extra_footprints = seen_refs.difference(&nl.refs).cloned().collect();
    report.missing_footprints = nl.refs.difference(&seen_refs).cloned().collect();

    Ok((apply_edits(content.to_string(), edits), report))
}

// ─── Tool handler ────────────────────────────────────────────────────────────

pub async fn handle_update_pcb_from_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    let board = get_path(args, "board")?;
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let tmp = std::env::temp_dir().join(format!("konnect-netlist-{}.net", std::process::id()));
    super::cli::export_netlist(&ctx.config.kicad_cli, &schematic, &tmp, "kicadsexpr").await?;
    let netlist_src = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(&tmp);

    let nl = parse_netlist(&netlist_src)?;
    let content = std::fs::read_to_string(&board)?;
    let (updated, report) = reconcile(&content, &nl)?;

    if !dry_run {
        write_atomic(&board, &updated)?;
    }

    Ok(CallToolResult::json(&json!({
        "applied": !dry_run,
        "report": report,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETLIST: &str = r#"
(export (version "E")
  (components
    (comp (ref "U1"))
    (comp (ref "J3")))
  (nets
    (net (code "1") (name "GND")
      (node (ref "U1") (pin "2"))
      (node (ref "J3") (pin "1")))
    (net (code "2") (name "/IF2_TX")
      (node (ref "U1") (pin "13"))
      (node (ref "J3") (pin "15")))))
"#;

    /// A board carrying the exact drift this tool was written for: a stale
    /// `IF2_TX` on the MCU pin, disjoint from `/IF2_TX` on the connector.
    ///
    /// Shaped like a real KiCad 10 file — tabs, and `(net "name")` with no id
    /// and no top-level table. The first version of this fixture used KiCad 9's
    /// `(net 7 "IF2_TX")`, every test passed, and the code was wrong for every
    /// board in the project.
    const BOARD: &str = "(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n\
\t(footprint \"L:U\"\n\t\t(property \"Reference\" \"U1\")\n\
\t\t(pad \"13\" smd rect\n\t\t\t(net \"IF2_TX\")\n\t\t)\n\
\t\t(pad \"2\" smd rect\n\t\t\t(net \"GND\")\n\t\t)\n\
\t\t(pad \"99\" smd rect\n\t\t\t(net \"GND\")\n\t\t)\n\t)\n\
\t(footprint \"L:J\"\n\t\t(property \"Reference\" \"J3\")\n\
\t\t(pad \"15\" smd rect\n\t\t\t(at 1 2 90)\n\t\t\t(layers \"F.Cu\" \"F.Mask\")\n\t\t\t(uuid \"abc\")\n\t\t)\n\
\t\t(pad \"1\" smd rect\n\t\t\t(net \"GND\")\n\t\t)\n\t)\n)";

    #[test]
    fn netlist_parses_nets_and_nodes() {
        let nl = parse_netlist(NETLIST).unwrap();
        assert_eq!(nl.nets, vec!["GND", "/IF2_TX"]);
        assert_eq!(
            nl.pad_nets
                .get(&("U1".into(), "13".into()))
                .map(String::as_str),
            Some("/IF2_TX")
        );
        assert!(nl.refs.contains("J3"));
    }

    #[test]
    fn split_net_is_rejoined_and_the_stale_name_disappears() {
        let nl = parse_netlist(NETLIST).unwrap();
        let (out, report) = reconcile(BOARD, &nl).unwrap();

        // `IF2_TX` and `/IF2_TX` are different strings, so in KiCad 10 they are
        // simply two different nets — this is the whole bug, and the fix is that
        // both ends now spell the same one.
        assert!(
            !out.contains("(net \"IF2_TX\")"),
            "stale bare name survived:\n{out}"
        );
        assert_eq!(
            out.matches("(net \"/IF2_TX\")").count(),
            2,
            "both the MCU pin and the connector pin must carry it"
        );
        assert!(report.stale_nets_removed.contains(&"IF2_TX".to_string()));
    }

    #[test]
    fn a_kicad_9_board_is_refused_rather_than_half_converted() {
        let legacy = BOARD.replace("(net \"GND\")", "(net 9 \"GND\")");
        let nl = parse_netlist(NETLIST).unwrap();
        let err = reconcile(&legacy, &nl).unwrap_err().to_string();
        assert!(err.contains("KiCad 9"), "unhelpful refusal: {err}");
    }

    #[test]
    fn a_board_already_in_step_is_left_byte_identical() {
        // The no-op case is the one that runs most often, and a tool that
        // churns the file on every run makes its own diffs useless.
        let nl = parse_netlist(NETLIST).unwrap();
        let (once, _) = reconcile(BOARD, &nl).unwrap();
        let (twice, report) = reconcile(&once, &nl).unwrap();
        assert_eq!(once, twice, "second run was not a no-op");
        assert_eq!(report.pads_assigned, 0);
        assert_eq!(report.pads_cleared, 0);
    }

    #[test]
    fn a_pad_the_schematic_does_not_mention_is_cleared() {
        let nl = parse_netlist(NETLIST).unwrap();
        let (out, report) = reconcile(BOARD, &nl).unwrap();
        // U1 pad 99 exists only on the board; its net must be dropped, which is
        // the half a merge cannot do.
        assert_eq!(report.pads_cleared, 1);
        // Bound the pad by its own closing paren rather than by "the next pad",
        // which is not in file order here — U1's pads run 13, 2, 99, so slicing
        // forward to `(pad "2"` overshoots into the next footprint.
        let start = out.find("(pad \"99\"").unwrap();
        let end = close_of(&out, start).unwrap();
        let pad99 = &out[start..=end];
        assert!(!pad99.contains("(net "), "pad 99 kept a net:\n{pad99}");
    }

    #[test]
    fn a_pad_with_no_net_child_gains_one() {
        let nl = parse_netlist(NETLIST).unwrap();
        let (out, _) = reconcile(BOARD, &nl).unwrap();
        // J3 pad 15 had no (net ...) at all and must gain /IF2_TX.
        let j3 = &out[out.find("\"Reference\" \"J3\"").unwrap()..];
        let pad15 = &j3[j3.find("(pad \"15\"").unwrap()..];
        assert!(
            pad15.contains("(net \"/IF2_TX\")"),
            "pad 15 not assigned:\n{pad15}"
        );
        // In KiCad's own field order — after (layers), before (uuid) — and
        // indented to match its siblings rather than to a fixed depth.
        assert!(
            pad15.contains(
                "(layers \"F.Cu\" \"F.Mask\")\n\t\t\t(net \"/IF2_TX\")\n\t\t\t(uuid \"abc\")"
            ),
            "net child in the wrong place:\n{pad15}"
        );
    }

    #[test]
    fn footprint_mismatches_are_reported_not_acted_on() {
        let nl = parse_netlist(NETLIST).unwrap();
        let before = BOARD.matches("(footprint ").count();
        let (out, report) = reconcile(BOARD, &nl).unwrap();
        assert_eq!(
            out.matches("(footprint ").count(),
            before,
            "footprints must never be added or removed"
        );
        assert!(report.extra_footprints.is_empty());
        assert!(report.missing_footprints.is_empty());
    }

    /// Run against a real project, which is the only thing that catches a
    /// format assumption being wrong — the synthetic fixtures above all passed
    /// against a KiCad 9 net table that no board in the wild still has.
    ///
    ///     KONNECT_TEST_BOARD=…/ModuleBase.kicad_pcb \
    ///     KONNECT_TEST_NETLIST=…/ModuleBase.net \
    ///     cargo test -p konnect-core real_board -- --ignored --nocapture
    #[test]
    #[ignore = "needs a real board; set KONNECT_TEST_BOARD and KONNECT_TEST_NETLIST"]
    fn real_board_round_trips() {
        let board_path = std::env::var("KONNECT_TEST_BOARD").expect("KONNECT_TEST_BOARD");
        let net_path = std::env::var("KONNECT_TEST_NETLIST").expect("KONNECT_TEST_NETLIST");
        let board = std::fs::read_to_string(&board_path).unwrap();
        let nl = parse_netlist(&std::fs::read_to_string(&net_path).unwrap()).unwrap();

        let (out, report) = reconcile(&board, &nl).unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());

        // Footprints are never touched.
        assert_eq!(
            out.matches("\t(footprint ").count(),
            board.matches("\t(footprint ").count()
        );
        // Idempotent on a real file, not just on a 14-line fixture.
        let (again, second) = reconcile(&out, &nl).unwrap();
        assert_eq!(out, again, "not idempotent on a real board");
        assert_eq!(second.pads_assigned, 0);
        assert_eq!(second.pads_cleared, 0);
        // Every net the schematic has must appear on some pad afterwards.
        for n in &nl.nets {
            assert!(out.contains(&format!("(net \"{n}\")")), "net {n} lost");
        }
    }

    /// The strongest check available: take a board KiCad's own F8 has already
    /// synced, drift it, repair it here, and require the result to be
    /// byte-identical to what KiCad produced. Anything this tool does
    /// differently from F8 — spacing, ordering, a dropped sibling — shows up as
    /// a diff.
    ///
    ///     KONNECT_TEST_BOARD=…/ModuleBase.kicad_pcb \
    ///     KONNECT_TEST_DRIFTED=…/drifted.kicad_pcb \
    ///     KONNECT_TEST_NETLIST=…/ModuleBase.net \
    ///     cargo test -p konnect-core real_board_drift -- --ignored --nocapture
    #[test]
    #[ignore = "needs a real board; see real_board_round_trips"]
    fn real_board_drift_is_repaired_exactly() {
        let pristine =
            std::fs::read_to_string(std::env::var("KONNECT_TEST_BOARD").unwrap()).unwrap();
        let drifted =
            std::fs::read_to_string(std::env::var("KONNECT_TEST_DRIFTED").unwrap()).unwrap();
        let nl = parse_netlist(
            &std::fs::read_to_string(std::env::var("KONNECT_TEST_NETLIST").unwrap()).unwrap(),
        )
        .unwrap();
        assert_ne!(
            pristine, drifted,
            "the drifted copy is not actually drifted"
        );

        let (repaired, report) = reconcile(&drifted, &nl).unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());

        if repaired != pristine {
            // Never assert_eq! two 800 kB strings — the panic message buries the
            // one line that differs under the whole file.
            let at = repaired
                .char_indices()
                .zip(pristine.chars())
                .find(|((_, a), b)| a != b)
                .map(|((i, _), _)| i)
                .unwrap_or(repaired.len().min(pristine.len()));
            let ctx = |s: &str| {
                let lo = s[..at.min(s.len())].rfind('\n').map(|p| p + 1).unwrap_or(0);
                let hi = s[lo..]
                    .char_indices()
                    .filter(|(_, c)| *c == '\n')
                    .nth(3)
                    .map(|(p, _)| lo + p)
                    .unwrap_or(s.len());
                s[lo..hi].to_string()
            };
            panic!(
                "repair does not reproduce KiCad's own F8 output; first difference at byte {at}\n\
                 --- kicad ---\n{}\n--- konnect ---\n{}",
                ctx(&pristine),
                ctx(&repaired)
            );
        }
    }

    #[test]
    fn parens_inside_quoted_strings_do_not_confuse_the_scanner() {
        // A descr with unbalanced parens is normal in KiCad footprints and
        // would break a naive depth counter.
        let board = BOARD.replace(
            "(property \"Reference\" \"U1\")",
            "(descr \"a )( pathological :-( description\")\n\t\t(property \"Reference\" \"U1\")",
        );
        let nl = parse_netlist(NETLIST).unwrap();
        let (out, _) = reconcile(&board, &nl).unwrap();
        assert!(out.contains("(net \"/IF2_TX\")"));
        assert!(out.contains("pathological"));
    }
}
