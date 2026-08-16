//! Tool trait definitions, ToolContext, and all toolset modules.

pub mod cli;
pub mod config;
pub mod design_review;
pub mod integration;
pub mod library;
pub mod manufacturing;
pub mod pcb_board;
pub mod pcb_components;
pub mod pcb_export;
pub mod pcb_routing;
pub mod pcb_sync;
pub mod project;
pub mod sch_analysis;
pub mod sch_batch;
pub mod sch_bridge;
pub mod sch_components;
pub mod sch_export;
pub mod sch_hierarchy;
pub mod sch_wiring;
pub mod schematic_builder;
pub mod svg_import;
pub mod templates;
pub mod verification;

use crate::mcp::protocol::{CallToolResult, McpToolDescription};
use crate::router::ToolRouter;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ─── Tool Handler Type ────────────────────────────────────────────────────────

pub type ToolHandlerFn = Arc<
    dyn Fn(
            &Value,
            Arc<ToolContext>,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<CallToolResult>> + Send>>
        + Send
        + Sync,
>;

// ─── ToolDef ─────────────────────────────────────────────────────────────────

/// A single tool definition: schema + async handler.
#[derive(Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub handler: ToolHandlerFn,
}

impl ToolDef {
    pub fn to_mcp_description(&self) -> McpToolDescription {
        McpToolDescription {
            name: self.name.to_string(),
            description: self.description.to_string(),
            input_schema: self.input_schema.clone(),
        }
    }
}

// Implement Debug manually because handler is not Debug
impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish()
    }
}

// ─── ToolContext ──────────────────────────────────────────────────────────────

/// Shared context passed to every tool handler.
/// Contains config, the tool router, lazily-initialized KiCAD clients, and the
/// per-call observer (used by `get_recent_calls` / `server_stats` meta-tools).
pub struct ToolContext {
    pub config: ServerConfig,
    pub router: Arc<ToolRouter>,
    pub observer: crate::observability::CallObserver,
    /// In-memory TTL cache for repeated JLCPCB parts-database queries.
    pub jlcpcb_cache: QueryCache,
}

impl ToolContext {
    /// Construct a context with an in-memory-only observer (no JSONL). Used by
    /// tests and by callers that don't need persistent call logs.
    pub fn new(config: ServerConfig, router: Arc<ToolRouter>) -> Self {
        ToolContext {
            config,
            router,
            observer: crate::observability::CallObserver::new(None),
            jlcpcb_cache: QueryCache::default(),
        }
    }

    /// Construct a context with a specific observer — wired in by `McpHandler`
    /// so the JSONL log and in-memory ring are shared across all tool calls.
    pub fn new_with_observer(
        config: ServerConfig,
        router: Arc<ToolRouter>,
        observer: crate::observability::CallObserver,
    ) -> Self {
        ToolContext {
            config,
            router,
            observer,
            jlcpcb_cache: QueryCache::default(),
        }
    }
}

// ─── QueryCache ───────────────────────────────────────────────────────────────

/// A small in-memory, TTL-based cache for repeated read-only query results
/// (JSON values keyed by a caller-constructed string). One instance lives on
/// `ToolContext` for the life of the server, shared across all tool calls.
pub struct QueryCache {
    ttl: std::time::Duration,
    entries: std::sync::Mutex<std::collections::HashMap<String, (Value, std::time::Instant)>>,
}

impl QueryCache {
    pub fn new(ttl: std::time::Duration) -> Self {
        QueryCache {
            ttl,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Returns a cached value for `key` if present and not yet expired.
    pub fn get(&self, key: &str) -> Option<Value> {
        let entries = self.entries.lock().unwrap();
        entries.get(key).and_then(|(value, inserted_at)| {
            if inserted_at.elapsed() < self.ttl {
                Some(value.clone())
            } else {
                None
            }
        })
    }

    /// Stores `value` under `key`, overwriting any existing (possibly expired) entry.
    pub fn put(&self, key: String, value: Value) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(key, (value, std::time::Instant::now()));
    }
}

impl Default for QueryCache {
    /// 5-minute TTL — long enough to skip redundant re-queries within a single
    /// design session, short enough that a `download_jlcpcb_database` refresh
    /// is reflected without needing an explicit cache-invalidation hook.
    fn default() -> Self {
        QueryCache::new(std::time::Duration::from_secs(300))
    }
}

// ─── ServerConfig ─────────────────────────────────────────────────────────────

/// Subset of the server configuration relevant to tool execution.
/// This is the config that flows from `konnect::Config` into the core crate.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub kicad_cli: String,
    pub kicad_binary: String,
    pub ipc_address: String,
    pub project_dir: Option<std::path::PathBuf>,
    pub jlcpcb_db_path: Option<std::path::PathBuf>,
    /// Auto-load a tool's toolset on call instead of returning
    /// `toolset_not_loaded`. Off by default (see `konnect::Config::auto_load_toolsets`).
    pub auto_load_toolsets: bool,
    /// Pre-load every toolset at startup so the first `tools/list` is
    /// complete. Off by default (see `konnect::Config::eager_toolsets`).
    pub eager_toolsets: bool,
}

/// Serialises tests that set `KICAD*_DIR`. Those are process-wide and read at
/// call time by `find_kicad_library_dirs`, so two such tests running
/// concurrently see each other's directories.
#[cfg(test)]
pub(crate) static KICAD_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod query_cache_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn miss_on_unknown_key() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn put_then_get_roundtrips() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        cache.put("key".to_string(), json!({ "count": 3 }));
        assert_eq!(cache.get("key"), Some(json!({ "count": 3 })));
    }

    #[test]
    fn entry_expires_after_ttl() {
        let cache = QueryCache::new(std::time::Duration::from_millis(10));
        cache.put("key".to_string(), json!("value"));
        assert_eq!(cache.get("key"), Some(json!("value")));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(cache.get("key").is_none());
    }

    #[test]
    fn put_overwrites_existing_entry() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        cache.put("key".to_string(), json!("first"));
        cache.put("key".to_string(), json!("second"));
        assert_eq!(cache.get("key"), Some(json!("second")));
    }
}

// ─── Helper macro for defining tools ─────────────────────────────────────────

/// Shorthand for building a ToolDef with a typed async handler function.
///
/// Usage:
/// ```rust,ignore
/// tool!(
///     "tool_name",
///     "Description of what it does.",
///     json_schema,        // serde_json::Value
///     |args, ctx| async move {
///         // handler body
///         Ok(CallToolResult::text("done"))
///     }
/// )
/// ```
#[macro_export]
macro_rules! tool {
    ($name:expr, $desc:expr, $schema:expr, $handler:expr) => {{
        let h: $crate::tools::ToolHandlerFn = std::sync::Arc::new(move |args, ctx| {
            let args = args.clone();
            let ctx = ctx.clone();
            Box::pin(async move { ($handler)(&args, &*ctx).await })
        });
        $crate::tools::ToolDef {
            name: $name,
            description: $desc,
            input_schema: $schema,
            handler: h,
        }
    }};
}

// ─── Argument helpers ─────────────────────────────────────────────────────────

/// Build a structured `InvalidArgument` CallToolResult. Used by the
/// `require_*` helpers so every handler that uses them emits structured
/// errors the client / observer can match on — no per-handler change needed.
fn invalid_arg(field: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.to_string(),
        },
        format!("Argument '{}' is invalid: {}", field, reason),
    )
}

/// Extract a required string argument, returning a structured
/// `InvalidArgument` error result if missing or not a string.
pub fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, CallToolResult> {
    args[key]
        .as_str()
        .ok_or_else(|| invalid_arg(key, "missing or not a string"))
}

/// Extract an optional string argument.
pub fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str()
}

/// Extract a required f64 argument. Returns a structured `InvalidArgument`
/// error result if missing or not a number.
pub fn require_f64(args: &Value, key: &str) -> Result<f64, CallToolResult> {
    args[key]
        .as_f64()
        .ok_or_else(|| invalid_arg(key, "missing or not a number"))
}

/// Extract an optional f64.
pub fn opt_f64(args: &Value, key: &str) -> Option<f64> {
    args[key].as_f64()
}

/// Extract a required path string and return it as a PathBuf, using
/// `anyhow::Error`. Use this variant with `?` inside handlers that return
/// `anyhow::Result`. The surrounding dispatch will stringify the error and
/// surface it as `ToolErrorKind::HandlerError`.
pub fn get_path(args: &Value, key: &str) -> anyhow::Result<std::path::PathBuf> {
    let s = args[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: '{}'", key))?;
    Ok(std::path::PathBuf::from(s))
}

/// Project name used in symbol/sheet `(instances (project "..." ...))` entries:
/// the schematic's file stem, matching what eeschema writes when it saves a
/// standalone root sheet.
pub fn project_name_for(sch_path: &std::path::Path) -> String {
    sch_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Minimal valid blank schematic, with a freshly generated root `(uuid ...)`.
/// The root UUID is mandatory: KiCAD's netlister resolves symbol instance
/// paths against it and silently forms no wire-only nets when it's missing.
pub fn blank_schematic_template() -> String {
    konnect_sexp::schematic::format_blank_schematic()
}

/// Root UUID of a loaded schematic, assigning a fresh one when the file
/// predates Konnect writing root UUIDs — the file is repaired on its next
/// overwrite. Instance paths are built as "/<root-uuid>[/<sheet-uuid>…]".
pub fn ensure_root_uuid(sch: &mut konnect_schematic_editor::Schematic) -> String {
    match &sch.uuid {
        Some(u) => u.clone(),
        None => {
            let u = konnect_sexp::writer::new_uuid();
            sch.uuid = Some(u.clone());
            u
        }
    }
}

/// Every pin placed on the sheet, paired with the transform that put it there.
///
/// Unit-aware: a multi-unit library symbol superimposes every unit's pins on
/// one placement, so an instance of unit 1 must not report unit 2's pins (#35).
pub(crate) fn placed_pins(
    tree: &konnect_sexp::SexpNode,
) -> Vec<(
    konnect_sexp::schematic::LibPin,
    konnect_sexp::geometry::PinTransform,
)> {
    use konnect_sexp::schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, find_lib_symbol,
    };
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut pins = Vec::new();
    for inst in extract_symbol_instances(tree) {
        // find_lib_symbol, not a lib_id match: an instance carrying a
        // (lib_name …) is a sheet-local derived symbol whose pins can sit
        // elsewhere than the base definition's (#143).
        let Some(sym) = find_lib_symbol(&lib_syms, &inst) else {
            continue;
        };
        let t = inst.pin_transform();
        pins.extend(
            extract_lib_pins_for_unit(sym, inst.unit)
                .into_iter()
                .map(|p| (p, t)),
        );
    }
    pins
}

/// All symbol pin connection points in a parsed schematic tree. These drive
/// junction insertion, and a dot dropped on a phantom pin where two wires
/// cross would short them — hence [`placed_pins`]' unit-awareness.
pub(crate) fn all_pin_endpoints(tree: &konnect_sexp::SexpNode) -> Vec<(f64, f64)> {
    placed_pins(tree)
        .into_iter()
        .map(|(p, t)| konnect_sexp::schematic::pin_endpoint(&p, t))
        .collect()
}

/// The direction leading away from the symbol body at `(x, y)`. `None` when
/// no pin sits there, or when stacked pins disagree about which way is out.
pub(crate) fn pin_outward_at(tree: &konnect_sexp::SexpNode, x: f64, y: f64) -> Option<f64> {
    use konnect_sexp::geometry::points_coincident;
    use konnect_sexp::schematic::{pin_endpoint, pin_outward_direction};
    let mut found: Option<f64> = None;
    for (pin, t) in placed_pins(tree) {
        let (px, py) = pin_endpoint(&pin, t);
        if !points_coincident(px, py, x, y, 0.01) {
            continue;
        }
        let outward = pin_outward_direction(&pin, t);
        match found {
            Some(d) if d != outward => return None,
            _ => found = Some(outward),
        }
    }
    found
}

/// The stub directions, as name, unit offset, and the angle that offset points
/// along. Schematic Y grows downward, so "up" is negative. `"right"` leads:
/// it is the fallback for an unknown name and for an unresolvable `"auto"`.
const STUB_DIRECTIONS: [(&str, f64, f64, f64); 4] = [
    ("right", 1.0, 0.0, 0.0),
    ("up", 0.0, -1.0, 90.0),
    ("left", -1.0, 0.0, 180.0),
    ("down", 0.0, 1.0, 270.0),
];

/// A resolved stub direction: which way the wire leaves the anchor, and how to
/// orient the label at its far end.
pub(crate) struct StubDirection {
    pub name: &'static str,
    /// Unit offset in schematic space (Y grows downward).
    pub dx: f64,
    pub dy: f64,
    pub label_rotation: f64,
}

/// Resolve a `direction` argument against an already-known outward direction.
/// `"auto"` follows `outward`, falling back to `"right"` — the default before
/// `"auto"` existed — when the caller could not determine one.
pub(crate) fn stub_direction(direction: &str, outward: Option<f64>) -> StubDirection {
    use konnect_sexp::schematic::horizontal_label_rotation;
    let row = match direction {
        // Outward angles are snapped to quadrants, so this compares exactly.
        "auto" => outward.and_then(|d| STUB_DIRECTIONS.iter().find(|r| r.3 == d)),
        name => STUB_DIRECTIONS.iter().find(|r| r.0 == name),
    }
    .unwrap_or(&STUB_DIRECTIONS[0]);
    StubDirection {
        name: row.0,
        dx: row.1,
        dy: row.2,
        label_rotation: horizontal_label_rotation(row.3),
    }
}

/// [`stub_direction`] for a caller holding only a coordinate. Naming a pin is
/// exact; matching one by position gives up when stacked pins there disagree.
pub(crate) fn resolve_stub_direction(
    direction: &str,
    anchor: (f64, f64),
    tree: &konnect_sexp::SexpNode,
) -> StubDirection {
    stub_direction(direction, pin_outward_at(tree, anchor.0, anchor.1))
}

/// Add junction dots for pins of `reference` that land mid-segment on a wire.
/// KiCad connects a pin mid-wire only through a junction dot (verified with
/// kicad-cli 10: a junction alone connects; splitting the wire is unnecessary).
/// Returns the junction positions added.
pub(crate) fn add_pin_midwire_junctions(
    sch_path: &std::path::Path,
    reference: &str,
) -> anyhow::Result<Vec<(f64, f64)>> {
    use konnect_sexp::geometry::{point_on_segment, points_coincident};
    use konnect_sexp::schematic::{
        extract_junctions, extract_lib_pins_for_unit, extract_symbol_instances, extract_wires,
        find_lib_symbol, pin_endpoint, read_schematic,
    };
    let tol = 0.01;
    let (_, tree) = read_schematic(sch_path)?;
    let wires = extract_wires(&tree);
    if wires.is_empty() {
        return Ok(Vec::new());
    }
    let junctions = extract_junctions(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut to_add: Vec<(f64, f64)> = Vec::new();
    for inst in extract_symbol_instances(&tree)
        .iter()
        .filter(|i| i.reference == reference)
    {
        let Some(sym) = find_lib_symbol(&lib_syms, inst) else {
            continue;
        };
        let t = inst.pin_transform();
        // Unit-aware for the same reason as all_pin_endpoints: this one writes
        // to the user's file, so a phantom-pin junction is a real defect.
        for pin in extract_lib_pins_for_unit(sym, inst.unit) {
            let (px, py) = pin_endpoint(&pin, t);
            let mid_wire = wires.iter().any(|w| {
                point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol)
                    && !points_coincident(px, py, w.x1, w.y1, tol)
                    && !points_coincident(px, py, w.x2, w.y2, tol)
            });
            let already = junctions
                .iter()
                .chain(to_add.iter())
                .any(|(jx, jy)| points_coincident(px, py, *jx, *jy, tol));
            if mid_wire && !already {
                to_add.push((px, py));
            }
        }
    }
    if !to_add.is_empty() {
        let mut sch = konnect_schematic_editor::Schematic::load(sch_path)?;
        for &(x, y) in &to_add {
            sch.add_junction(x, y);
        }
        sch.overwrite()?;
    }
    Ok(to_add)
}

/// A symbol-instance property positioned in absolute sheet coordinates, with
/// eeschema's default 1.27mm font. The `(at)` node is mandatory: a property
/// written without one is defaulted to the sheet origin by KiCAD, which is how
/// every `#PWR` reference used to pile up in the top-left corner (PR #95).
///
/// Hidden properties get KiCAD 10's property-level `(hide yes)` — a sibling
/// before `(effects)`, exactly as eeschema writes instances (PR #96); the
/// legacy hide-inside-effects form renders the same but round-trips dirty.
pub(crate) fn positioned_property(
    name: &str,
    value: &str,
    x: f64,
    y: f64,
    rotation: f64,
    hide: bool,
) -> konnect_schematic_editor::Property {
    use konnect_schematic_editor::sexp::{atom, SexpNode};
    use konnect_schematic_editor::types::fmt_f64;

    let mut prop = konnect_schematic_editor::Property::new(name, value);
    prop.sub_nodes.push(SexpNode::List(vec![
        atom("at"),
        atom(fmt_f64(x)),
        atom(fmt_f64(y)),
        atom(fmt_f64(rotation)),
    ]));
    if hide {
        prop.sub_nodes
            .push(SexpNode::List(vec![atom("hide"), atom("yes")]));
    }
    prop.sub_nodes.push(SexpNode::List(vec![
        atom("effects"),
        SexpNode::List(vec![
            atom("font"),
            SexpNode::List(vec![atom("size"), atom("1.27"), atom("1.27")]),
        ]),
    ]));
    prop
}

// ─── Schematic text helpers ──────────────────────────────────────────────────

/// Byte range of the placed `(symbol …)` block whose Reference property is
/// `reference`, for the text-editing tool paths.
///
/// Works regardless of indentation — eeschema saves with tabs, this crate's
/// writer uses two spaces — and skips library definitions inside `lib_symbols`,
/// which carry a Reference property of their own (`"R"`, `"#PWR"`, or whatever
/// a hand-authored library sets) but never a `lib_id`. Only placed instances
/// have one, so that's the discriminator.
pub fn find_symbol_instance_block(content: &str, reference: &str) -> Option<(usize, usize)> {
    find_all_symbol_instance_blocks(content, reference)
        .into_iter()
        .next()
}

/// Byte ranges of *every* placed `(symbol …)` block whose Reference property is
/// `reference`, in file order.
///
/// A multi-unit part is placed as one instance **per unit**, and every instance
/// repeats the same reference — a 74HC14 is seven `U6` blocks. Anything the
/// units share rather than own (a field value, the part's very existence) has to
/// be applied to all of them: eeschema writes a field edit into every unit, and
/// deleting one unit's block leaves the rest behind as orphans. Use this rather
/// than [`find_symbol_instance_block`] wherever the operation is about the
/// *component*; the singular form is for operations about one placement.
pub fn find_all_symbol_instance_blocks(content: &str, reference: &str) -> Vec<(usize, usize)> {
    let ref_search = format!(r#"(property "Reference" "{reference}""#);
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut from = 0usize;

    while let Some(rel) = content[from..].find(&ref_search) {
        let ref_pos = from + rel;
        if let Some((start, end)) =
            konnect_sexp::writer::find_enclosing_block(content, "symbol", ref_pos)
        {
            // Skip lib_symbols definitions: they carry a Reference property of
            // their own but never a lib_id.
            if content[start..end].contains("(lib_id ") && !blocks.iter().any(|&(s, _)| s == start)
            {
                blocks.push((start, end));
            }
        }
        from = ref_pos + ref_search.len();
    }
    blocks
}

/// One `(path … (reference "J3"))` entry from a symbol's `(instances)` block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InstanceRef {
    /// The `(project "…")` this path belongs to. Empty string is legal and
    /// common — KiCad writes `(project "")` for paths it can no longer
    /// attribute, and a file can carry several such blocks.
    pub project: String,
    /// The sheet path, `/<uuid>/<uuid>`.
    pub path: String,
    pub reference: String,
}

/// Every reference a placed symbol answers to, read from its `(instances)`.
///
/// A symbol's own `(property "Reference" …)` is a **cache of one** of these,
/// not the truth. When a sheet is instantiated more than once — four interface
/// sheets sharing one `Interface.kicad_sch` — the single symbol block in that
/// file is J2, J3, J4 and J7 simultaneously, and the property happens to hold
/// whichever one eeschema wrote last.
pub fn symbol_instance_refs(content: &str, block: (usize, usize)) -> Vec<InstanceRef> {
    let (start, end) = block;
    let Some(rel) = content[start..end].find("(instances") else {
        return Vec::new();
    };
    let inst_start = start + rel;
    let bytes = content.as_bytes();

    let mut out = Vec::new();
    let mut project = String::new();
    let mut path = String::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = inst_start;

    // Read a quoted string starting at the first `"` at or after `from`.
    let quoted = |from: usize| -> Option<(String, usize)> {
        let q = content[from..end].find('"')? + from + 1;
        let e = content[q..end].find('"')? + q;
        Some((content[q..e].to_string(), e))
    };

    while i < end {
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
                let rest = &content[i + 1..end.min(i + 32)];
                let n = rest
                    .find(|ch: char| ch.is_whitespace() || ch == '(' || ch == ')')
                    .unwrap_or(0);
                match &rest[..n] {
                    "project" => {
                        if let Some((p, _)) = quoted(i) {
                            project = p;
                        }
                    }
                    "path" => {
                        if let Some((p, _)) = quoted(i) {
                            path = p;
                        }
                    }
                    "reference" => {
                        if let Some((r, _)) = quoted(i) {
                            out.push(InstanceRef {
                                project: project.clone(),
                                path: path.clone(),
                                reference: r,
                            });
                        }
                    }
                    _ => {}
                }
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Byte ranges of every placed symbol block that answers to `reference`,
/// whether by its cached Reference property **or** by an entry in its
/// `(instances)` block.
///
/// Strictly wider than [`find_all_symbol_instance_blocks`]: everything that
/// resolved before still resolves, first and in the same order. The addition is
/// the reference that exists only in `(instances)`, which is the common case in
/// a sheet instantiated more than once and which previously came back "not
/// found" — the caller reads J3 off the netlist, asks for J3, and is told no
/// such component exists while looking straight at it.
pub fn find_symbol_blocks_by_any_reference(content: &str, reference: &str) -> Vec<(usize, usize)> {
    let mut blocks = find_all_symbol_instance_blocks(content, reference);

    let mut from = 0usize;
    while let Some(rel) = content[from..].find("(lib_id ") {
        let pos = from + rel;
        from = pos + "(lib_id ".len();
        let Some((start, end)) = konnect_sexp::writer::find_enclosing_block(content, "symbol", pos)
        else {
            continue;
        };
        if blocks.iter().any(|&(s, _)| s == start) {
            continue;
        }
        if symbol_instance_refs(content, (start, end))
            .iter()
            .any(|r| r.reference == reference)
        {
            blocks.push((start, end));
        }
    }
    blocks
}

#[cfg(test)]
mod symbol_block_tests {
    use super::*;

    /// Instance blocks as eeschema writes them: tab-indented, and preceded by a
    /// lib_symbols definition carrying its own Reference property.
    const EESCHEMA_STYLE: &str = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\"\n\t\t\t\t(at 2.032 0 90)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 102 82 0)\n\t\t)\n\t)\n)\n";

    /// Same shape, two-space indented, as this crate's writer emits.
    const KONNECT_STYLE: &str = "(kicad_sch\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\"\n        (at 2.032 0 90)\n      )\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 100 80 0)\n    (property \"Reference\" \"R1\"\n      (at 102 78 0)\n    )\n  )\n)\n";

    /// One symbol in a sheet instantiated four times in one project and four
    /// times in another, shaped like the real thing: the cached Reference says
    /// "J2", and J3/J4/J7 exist only inside `(instances)`. Two separate
    /// `(project "")` blocks, because KiCad really does write those.
    const SHARED_SHEET: &str = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Connector_Generic:Conn_01x16\"\n\
\t\t\t(property \"Reference\" \"J\"\n\t\t\t\t(at 0 0 0)\n\t\t\t)\n\t\t)\n\t)\n\
\t(symbol\n\t\t(lib_id \"Connector_Generic:Conn_01x16\")\n\t\t(unit 1)\n\
\t\t(property \"Reference\" \"J2\"\n\t\t\t(at 0 0 0)\n\t\t)\n\
\t\t(instances\n\
\t\t\t(project \"ModuleBase\"\n\
\t\t\t\t(path \"/aaa/p1\"\n\t\t\t\t\t(reference \"J3\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\
\t\t\t\t(path \"/aaa/p2\"\n\t\t\t\t\t(reference \"J7\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\
\t\t\t\t(path \"/aaa/p3\"\n\t\t\t\t\t(reference \"J4\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\
\t\t\t\t(path \"/aaa/p4\"\n\t\t\t\t\t(reference \"J2\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\
\t\t\t(project \"\"\n\
\t\t\t\t(path \"/bbb\"\n\t\t\t\t\t(reference \"J10\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\
\t\t\t(project \"PowerModule\"\n\
\t\t\t\t(path \"/ccc/p1\"\n\t\t\t\t\t(reference \"J6\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\
\t\t\t(project \"\"\n\
\t\t\t\t(path \"/ddd/p1\"\n\t\t\t\t\t(reference \"J10\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n)\n";

    #[test]
    fn instance_refs_are_read_with_their_projects() {
        let block = find_symbol_instance_block(SHARED_SHEET, "J2").expect("J2");
        let refs = symbol_instance_refs(SHARED_SHEET, block);
        assert_eq!(refs.len(), 7, "one entry per path: {refs:#?}");
        assert_eq!(refs[0].project, "ModuleBase");
        assert_eq!(refs[0].reference, "J3");
        assert_eq!(refs[0].path, "/aaa/p1");
        // A second (project "") block must not be folded into the first, and
        // must not inherit the project name that preceded it.
        assert_eq!(refs[4].project, "");
        assert_eq!(refs[5].project, "PowerModule");
        assert_eq!(refs[6].project, "");
    }

    #[test]
    fn a_reference_that_exists_only_in_instances_resolves() {
        // The bug this closes: J3 is a real reference on a real component, and
        // the caller reads it straight off the netlist.
        for r in ["J3", "J7", "J4", "J6", "J10"] {
            assert!(
                !find_symbol_blocks_by_any_reference(SHARED_SHEET, r).is_empty(),
                "{r} did not resolve"
            );
        }
        // ...and the cached property still resolves, first and unchanged.
        let by_prop = find_all_symbol_instance_blocks(SHARED_SHEET, "J2");
        let by_any = find_symbol_blocks_by_any_reference(SHARED_SHEET, "J2");
        assert_eq!(by_prop, by_any);
        assert!(!by_any.is_empty());
    }

    #[test]
    fn every_alias_of_one_symbol_resolves_to_the_same_block() {
        let want = find_symbol_blocks_by_any_reference(SHARED_SHEET, "J2");
        for r in ["J3", "J7", "J4", "J6"] {
            assert_eq!(
                find_symbol_blocks_by_any_reference(SHARED_SHEET, r),
                want,
                "{r} resolved to a different block"
            );
        }
    }

    #[test]
    fn a_reference_nobody_has_still_does_not_resolve() {
        assert!(find_symbol_blocks_by_any_reference(SHARED_SHEET, "J99").is_empty());
        assert!(symbol_instance_refs(EESCHEMA_STYLE, find_symbol_instance_block(EESCHEMA_STYLE, "R1").unwrap()).is_empty(),
                "a symbol with no (instances) block yields no refs, not a panic");
    }

    #[test]
    fn finds_instance_in_tab_indented_file() {
        let (start, end) = find_symbol_instance_block(EESCHEMA_STYLE, "R1").expect("R1 block");
        let block = &EESCHEMA_STYLE[start..end];
        assert!(block.starts_with("(symbol"));
        assert!(block.contains("(lib_id \"Device:R\")"));
        assert!(block.contains("\"R1\""));
        assert!(
            block.contains("\"10k\""),
            "block must span the whole symbol"
        );
    }

    #[test]
    fn finds_instance_in_space_indented_file() {
        let (start, end) = find_symbol_instance_block(KONNECT_STYLE, "R1").expect("R1 block");
        assert!(KONNECT_STYLE[start..end].contains("(lib_id \"Device:R\")"));
    }

    #[test]
    fn library_definition_is_not_mistaken_for_an_instance() {
        // A hand-authored library whose default Reference matches a placed
        // instance's designator must not shadow the instance.
        let sch = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Custom:Thing\"\n\t\t\t(property \"Reference\" \"U1\"\n\t\t\t\t(at 0 0 0)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Custom:Thing\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 5 5 0)\n\t\t)\n\t)\n)\n";
        let (start, end) = find_symbol_instance_block(sch, "U1").expect("instance");
        assert!(
            sch[start..end].contains("(lib_id "),
            "must skip the lib_symbols definition and return the placed instance"
        );
    }

    #[test]
    fn unknown_reference_is_none() {
        assert!(find_symbol_instance_block(EESCHEMA_STYLE, "R99").is_none());
    }

    #[test]
    fn reference_prefix_does_not_match_longer_designator() {
        // "R1" must not match the R12 instance.
        let sch = "(kicad_sch\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(property \"Reference\" \"R12\"\n\t\t\t(at 1 1 0)\n\t\t)\n\t)\n)\n";
        assert!(find_symbol_instance_block(sch, "R1").is_none());
    }
}

#[cfg(test)]
mod arg_helper_tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use serde_json::json;

    #[test]
    fn require_str_missing_produces_structured_invalid_argument() {
        let args = json!({});
        let err = require_str(&args, "path").expect_err("should fail");
        assert!(err.is_error);
        assert_eq!(
            extract_error_kind(&err).as_deref(),
            Some("invalid_argument")
        );
        // The body carries the field name so clients can branch.
        let body = match &err.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["field"], "path");
    }

    #[test]
    fn require_f64_non_number_produces_structured_invalid_argument() {
        let args = json!({ "x": "not a number" });
        let err = require_f64(&args, "x").expect_err("should fail");
        assert_eq!(
            extract_error_kind(&err).as_deref(),
            Some("invalid_argument")
        );
    }

    #[test]
    fn require_str_present_returns_value() {
        let args = json!({ "name": "ok" });
        let v = require_str(&args, "name").expect("should parse");
        assert_eq!(v, "ok");
    }
}

// ─── KiCAD config directory detection ────────────────────────────────────────

/// Find the KiCAD user config directory by probing for installed version directories.
/// Checks versions in descending order: 10.0, 9.0, 8.0, then bare "kicad".
pub fn kicad_config_dir() -> std::path::PathBuf {
    let base = kicad_config_base();
    let versions = ["10.0", "9.0", "8.0"];
    for ver in &versions {
        let dir = base.join(ver);
        if dir.is_dir() {
            return dir;
        }
    }
    // Fallback: bare kicad dir or 10.0 (will be created on first use)
    base.join("10.0")
}

/// Platform-specific base directory for KiCAD configs.
fn kicad_config_base() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        std::path::PathBuf::from(appdata).join("kicad")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home)
            .join("Library")
            .join("Preferences")
            .join("kicad")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home).join(".config").join("kicad")
    }
}

// ─── lib_symbols embedding ──────────────────────────────────────────────────

/// Structured "this lib_id doesn't exist" error, with did-you-mean hints —
/// silently accepting an unresolvable lib_id writes a netlist-invisible
/// component with an empty pin list (#34).
pub fn lib_symbol_not_found_error(
    lib_id: &str,
    src: &dyn konnect_schematic_editor::library::SymbolLibrarySource,
) -> CallToolResult {
    let library = lib_id.split(':').next().unwrap_or(lib_id);
    let mut msg = if !konnect_schematic_editor::library::library_exists(library, src) {
        // Naming only KICAD10_SYMBOL_DIR misleads when the library *is*
        // registered — the tables are the primary source.
        format!(
            "Library '{}' not found in the project or global sym-lib-table, nor as \
             '{}.kicad_symdir'/'{}.kicad_sym' in the installed KiCad symbol \
             libraries (lib_id '{}'). Register it with register_symbol_library, \
             or set KICAD10_SYMBOL_DIR for a non-standard install.",
            library, library, library, lib_id
        )
    } else {
        format!(
            "Library symbol '{}' not found in library '{}'.",
            lib_id, library
        )
    };
    let suggestions = konnect_schematic_editor::library::suggest_symbols(lib_id, 3, src);
    if !suggestions.is_empty() {
        msg.push_str(&format!(
            " Did you mean: {}? (KiCAD 10 renamed several older symbol names)",
            suggestions.join(", ")
        ));
    }
    CallToolResult::error(msg)
}

/// Insert a symbol definition into the schematic's lib_symbols section.
/// Creates the lib_symbols section if it doesn't exist. Skips if already present.
///
/// Returns `false` when `lib_id` cannot be resolved — callers must surface
/// that as an error rather than writing a definition-less instance (#34).
#[must_use]
pub fn ensure_lib_symbol_in_schematic(
    content: &mut String,
    lib_id: &str,
    src: &dyn konnect_schematic_editor::library::SymbolLibrarySource,
) -> bool {
    // Check if already present
    let lib_id_check = format!("(symbol \"{}\"", lib_id);
    if content.contains(&lib_id_check) {
        return true;
    }

    // Flattened: a derived symbol must be embedded with its parent's units
    // copied in, not as a stub kicad-cli can't netlist (#35).
    let sym_def = match konnect_schematic_editor::library::resolve_lib_symbol_flattened(lib_id, src)
    {
        Some(s) => s,
        None => return false,
    };

    // Ensure lib_symbols section exists
    if !content.contains("(lib_symbols") {
        if let Some(insert_after) = content.find(")\n") {
            content.insert_str(insert_after + 2, "\n\t(lib_symbols\n\t)\n");
        }
    }

    // Find the closing paren of lib_symbols and insert before it
    if let Some(ls_start) = content.find("(lib_symbols") {
        let mut depth = 0i32;
        let mut ls_end = ls_start;
        for (i, ch) in content[ls_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        ls_end = ls_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let indented = sym_def
            .lines()
            .map(|l| {
                if l.is_empty() {
                    String::new()
                } else {
                    format!("\t\t{}", l)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        content.insert_str(ls_end, &format!("\n{}\n\t", indented));
    }
    true
}

/// Roots under which KiCAD ships its bundled libraries — the directory that
/// directly contains `symbols/`, `footprints/` and `3dmodels/`.
fn kicad_share_roots() -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        // Keep these majors in step with the ones find_kicad_library_dirs
        // reads environment variables for. A major listed there but missing
        // here is invisible on any machine where KiCad did not export its
        // variable — which is every machine where Konnect was not launched
        // by KiCad.
        for c in [
            r"C:\KiCad\10.0\share\kicad",
            r"C:\Program Files\KiCad\10.0\share\kicad",
            r"C:\KiCad\9.0\share\kicad",
            r"C:\Program Files\KiCad\9.0\share\kicad",
            r"C:\KiCad\8.0\share\kicad",
            r"C:\Program Files\KiCad\8.0\share\kicad",
        ] {
            roots.push(std::path::PathBuf::from(c));
        }
    }
    #[cfg(target_os = "macos")]
    {
        // KiCad on macOS ships its libraries inside the app bundle.
        roots.push(std::path::PathBuf::from(
            "/Applications/KiCad/KiCad.app/Contents/SharedSupport",
        ));
        roots.push(std::path::PathBuf::from("/usr/local/share/kicad"));
        // Homebrew (Apple Silicon prefix)
        roots.push(std::path::PathBuf::from("/opt/homebrew/share/kicad"));
        if let Ok(home) = std::env::var("HOME") {
            // Per-user install (KiCad.app dragged into ~/Applications)
            roots.push(
                std::path::PathBuf::from(home)
                    .join("Applications/KiCad/KiCad.app/Contents/SharedSupport"),
            );
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        roots.push(std::path::PathBuf::from("/usr/share/kicad"));
        roots.push(std::path::PathBuf::from("/usr/local/share/kicad"));
        roots.push(std::path::PathBuf::from("/opt/kicad/share/kicad"));
        // Flatpak: system-wide and per-user installs
        roots.push(std::path::PathBuf::from(
            "/var/lib/flatpak/app/org.kicad.KiCad/current/active/files/share/kicad",
        ));
        if let Ok(home) = std::env::var("HOME") {
            roots.push(
                std::path::PathBuf::from(&home).join(
                    ".local/share/flatpak/app/org.kicad.KiCad/current/active/files/share/kicad",
                ),
            );
        }
        // Snap
        roots.push(std::path::PathBuf::from(
            "/snap/kicad/current/usr/share/kicad",
        ));
    }

    roots.retain(|p| p.is_dir());
    roots
}

/// Find directories holding a bundled KiCAD library kind — `"symbols"`,
/// `"footprints"` or `"3dmodels"`.
///
/// The matching environment variable wins when KiCad has exported it (it does
/// so for plugins); otherwise the well-known install locations are searched,
/// newest KiCad first. The names are not a plain uppercasing of `kind` — they
/// are singular, and the 3D one is not a word:
///
/// | `kind`        | variable                |
/// |---------------|-------------------------|
/// | `symbols`     | `KICAD<major>_SYMBOL_DIR`    |
/// | `footprints`  | `KICAD<major>_FOOTPRINT_DIR` |
/// | `3dmodels`    | `KICAD<major>_3DMODEL_DIR`   |
pub(crate) fn find_kicad_library_dirs(kind: &str) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut push = |p: std::path::PathBuf| {
        if p.is_dir() && !dirs.contains(&p) {
            dirs.push(p);
        }
    };

    if let Some(suffix) = kicad_env_suffix(kind) {
        for major in ["10", "9", "8"] {
            // var_os, not var: a directory whose name is not valid Unicode is
            // still a directory KiCad may have pointed us at, and `var` reports
            // those as absent — silently falling back to the install roots, or
            // to nothing, on exactly the machines where the variable was the
            // only correct answer.
            if let Some(dir) = std::env::var_os(format!("KICAD{major}_{suffix}")) {
                push(std::path::PathBuf::from(dir));
            }
        }
    }
    for root in kicad_share_roots() {
        push(root.join(kind));
    }
    dirs
}

/// The `KICAD<major>_…` environment-variable suffix naming a library kind.
fn kicad_env_suffix(kind: &str) -> Option<&'static str> {
    match kind {
        "symbols" => Some("SYMBOL_DIR"),
        "footprints" => Some("FOOTPRINT_DIR"),
        "3dmodels" => Some("3DMODEL_DIR"),
        _ => None,
    }
}
