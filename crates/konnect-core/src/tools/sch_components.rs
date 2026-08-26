//! `sch_components` toolset — add, edit, move, rotate, delete schematic symbols.
//!
//! Simple CRUD operations use `konnect_schematic_editor` (cse) for structured
//! round-trip parsing.  Pin coordinate math still delegates to
//! `konnect_sexp::geometry::transform_pin`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_all_symbol_instance_blocks, find_symbol_instance_block, get_path, opt_f64, opt_str,
    project_name_for, require_f64, require_str, ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    commit_command,
    geometry::{point_on_segment, points_coincident, snap_point},
    parse_sexp,
    schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, find_lib_symbol, pin_endpoint,
        pin_outward_direction, read_schematic,
    },
    writer::{
        apply_edits, new_uuid, read_consistent, write_atomic_if_unchanged, write_new_atomic,
        SexpEdit,
    },
    ItemId, SchematicCommand,
};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_schematic",
            "Create a new blank .kicad_sch schematic file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path for the new .kicad_sch file" }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_create_schematic(args, ctx).await }
        ),
        tool!(
            "add_schematic_component",
            "Add a symbol from a KiCAD library to the schematic. The symbol is snapped \
             to the 1.27mm schematic grid. Specify position in schematic mm coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "lib_id": { "type": "string", "description": "Library:Symbol (e.g. 'Device:R')" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "rotation": { "type": "number", "description": "Rotation in degrees (0/90/180/270)", "default": 0 },
                    "reference": { "type": "string", "description": "Optional override for reference designator" },
                    "value": { "type": "string", "description": "Optional override for value field" },
                    "unit": { "type": "integer", "description": "Unit number for multi-unit symbols (gate/part selection). Default 1.", "default": 1 }
                },
                "required": ["schematic", "lib_id", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_component(args, ctx).await }
        ),
        tool!(
            "delete_schematic_component",
            "Remove a symbol instance from the schematic by its reference designator.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator (e.g. 'R1')" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_delete_schematic_component(args, ctx).await }
        ),
        tool!(
            "edit_schematic_component",
            "Update fields (Reference, Value, Footprint, custom properties) of a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Current reference designator" },
                    "new_reference": { "type": "string", "description": "New reference designator (optional)" },
                    "value": { "type": "string", "description": "New value (optional)" },
                    "footprint": { "type": "string", "description": "New footprint (optional)" },
                    "datasheet": { "type": "string", "description": "New datasheet URL (optional)" },
                    "fields": {
                        "type": "object",
                        "description": "Additional property fields to set as key:value pairs"
                    }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_edit_schematic_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_component",
            "Get all properties, position, and pin locations for a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_component(args, ctx).await }
        ),
        tool!(
            "list_schematic_components",
            "List all symbol instances in a schematic with their positions, values, \
             footprints, and pin locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_list_schematic_components(args, ctx).await }
        ),
        tool!(
            "move_schematic_component",
            "Move a symbol to a new position. Does NOT adjust connected wires.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number", "description": "New X position in mm" },
                    "y": { "type": "number", "description": "New Y position in mm" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_schematic_component(args, ctx).await }
        ),
        tool!(
            "rotate_schematic_component",
            "Rotate a symbol by setting its absolute rotation angle (0/90/180/270).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation": { "type": "number", "description": "Absolute rotation in degrees" }
                },
                "required": ["schematic", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_schematic_component(args, ctx).await }
        ),
        tool!(
            "move_connected",
            "Move a symbol and drag everything attached to its pins with it, so the move preserves \
             connectivity instead of silently disconnecting the part. Wire endpoints sitting on a \
             pin follow it (the wire stretches from its far end); net/global/hierarchical labels, \
             no-connect flags and junctions on a pin travel with it. Attachment is exact \
             coincidence, matching KiCAD, so nothing merely near a pin is touched. One exception \
             is reported rather than guessed: a junction where a wire passes *through* the pin \
             point is left in place, because it holds that T together — see junctions_left_behind \
             in the result. Prefer this over move_schematic_component for any symbol that is \
             already wired.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number" },
                    "y": { "type": "number" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_connected(args, ctx).await }
        ),
        tool!(
            "move_region",
            "Move all symbols within a bounding box by a given offset.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number", "description": "Region bounding box min X" },
                    "y1": { "type": "number", "description": "Region bounding box min Y" },
                    "x2": { "type": "number", "description": "Region bounding box max X" },
                    "y2": { "type": "number", "description": "Region bounding box max Y" },
                    "dx": { "type": "number", "description": "X offset to move by" },
                    "dy": { "type": "number", "description": "Y offset to move by" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2", "dx", "dy"]
            }),
            |args, ctx| async move { handle_move_region(args, ctx).await }
        ),
        tool!(
            "annotate_schematic",
            "Run kicad-cli to auto-assign reference designators (R? → R1, U? → U1, etc.).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_annotate_schematic(args, ctx).await }
        ),
        tool!(
            "get_schematic_pin_locations",
            "Get the exact schematic-space (X,Y) coordinates of every pin on a symbol, \
             accounting for rotation and mirroring. Uses the canonical pin transform. \
             Each pin also reports 'orientation_degrees', the direction leading away \
             from the symbol body (0 = east) — a net label at the pin must read that \
             way or its text runs back over the symbol's pin names — and 'length_mm'.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_pin_locations(args, ctx).await }
        ),
        tool!(
            "batch_get_schematic_pin_locations",
            "Get pin locations for multiple components in a single file read. Reports the \
             same per-pin fields as get_schematic_pin_locations, including \
             'orientation_degrees' and 'length_mm'.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators"
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_get_pin_locations(args, ctx).await }
        ),
        tool!(
            "add_component_annotation",
            "Add a custom property (annotation) to a symbol instance in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'R1')" },
                    "key": { "type": "string", "description": "Property name" },
                    "value": { "type": "string", "description": "Property value" }
                },
                "required": ["schematic", "reference", "key", "value"]
            }),
            |args, ctx| async move { handle_add_component_annotation(args, ctx).await }
        ),
        tool!(
            "group_components",
            "Add a group property to multiple components in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators to group"
                    },
                    "group_name": { "type": "string", "description": "Group name to assign" }
                },
                "required": ["schematic", "references", "group_name"]
            }),
            |args, ctx| async move { handle_group_components(args, ctx).await }
        ),
        tool!(
            "replace_component",
            "Replace a component's lib_id with a new library symbol (swap the component type). \
             The reference is resolved through the symbol's (instances) block as well as its \
             cached Reference property, so a reference that exists only in a sheet instantiated \
             more than once — where one symbol is J2, J3, J4 and J7 at the same time — resolves \
             instead of coming back 'not found'. Because those are one placement, the swap \
             changes all of them; the returned `also_affects` lists the other references and \
             `shared_instances` counts them. The (instances) block itself is preserved untouched.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'U1')" },
                    "new_lib_id": { "type": "string", "description": "New Library:Symbol identifier (e.g. 'Device:C')" },
                    "unit": { "type": "integer", "description": "Optional unit number for multi-unit symbols; validated against the new symbol's unit count. When omitted the existing unit is kept." }
                },
                "required": ["schematic", "reference", "new_lib_id"]
            }),
            |args, ctx| async move { handle_replace_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_view",
            "Render the schematic to a PNG image (base64-encoded) via kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_schematic_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_create_schematic(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;
    // Build a minimal valid schematic and save via cse's atomic writer.
    let template = crate::tools::blank_schematic_template();
    // Write the template then immediately load/save through cse so the file
    // is normalised to cse's writer output format.
    write_new_atomic(&path, &template)?;
    let sch = cse::Schematic::load(&path)?;
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "created": path.display().to_string() }),
    ))
}

async fn handle_add_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let lib_id = match require_str(args, "lib_id") {
        Ok(s) => s.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let rotation = opt_f64(args, "rotation").unwrap_or(0.0);
    let reference = opt_str(args, "reference");
    let value = opt_str(args, "value");
    let unit = opt_f64(args, "unit").unwrap_or(1.0) as u32;
    let ref_str = reference.unwrap_or("?");

    // Load via konnect-schematic-editor
    let mut sch = cse::Schematic::load(&sch_path)?;

    // Instance paths must resolve against the root sheet UUID — KiCAD's
    // netlister silently forms no wire-only nets for symbols whose path
    // doesn't. Called unconditionally so a file predating root UUIDs is
    // repaired even when its slots are inherited below.
    crate::tools::ensure_root_uuid(&mut sch);
    let slots = sheet_slots_for(&mut sch, &sch_path);
    let mut used = crate::tools::collect_used_references(&sch_path);
    let refs = refs_for_slots(ref_str, &slots, &mut used);

    let result = match place_one_component(
        &mut sch,
        &slots,
        &refs,
        &lib_id,
        x,
        y,
        rotation,
        value,
        unit,
        &crate::tools::library::KiCadSymbolSource::for_file(&sch_path),
    ) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    sch.overwrite()?;

    // A pin landing mid-segment on an existing wire needs a junction dot, or
    // KiCad's netlister treats it as unconnected. Runs after the write because
    // it re-reads the saved file; `place_one_component` stays pure so the batch
    // path can do one junction pass for the whole batch instead of one per part.
    let mut result = result;
    let junctions = crate::tools::add_pin_midwire_junctions(&sch_path, ref_str)?;
    result["junctions_added"] = json!(junctions
        .iter()
        .map(|(x, y)| json!({ "x": x, "y": y }))
        .collect::<Vec<_>>());

    Ok(CallToolResult::json(&result))
}

/// Where a symbol added to this sheet must be instantiated, and what it is
/// called at each position.
///
/// A sheet instantiated once behaves exactly as before: one slot, and the
/// caller's reference used verbatim. A sheet instantiated more than once gets
/// one slot per instantiation and a distinct designator for each — writing a
/// single path there leaves the symbol unannotated in the real project and
/// absent from its netlist, which is the defect this exists to prevent.
///
/// A seed carrying no number (`"?"`, the unannotated default) allocates
/// nothing and leaves every instance `"?"`, matching what eeschema shows for a
/// symbol awaiting annotation.
pub(crate) fn sheet_slots_for(
    sch: &mut cse::Schematic,
    sch_path: &std::path::Path,
) -> Vec<crate::tools::InstanceSlot> {
    let discovered = crate::tools::sheet_instance_slots(sch);
    if !discovered.is_empty() {
        return discovered;
    }
    // No symbols to inherit from: treat the sheet as its own root, which is
    // what eeschema writes for a standalone save.
    vec![crate::tools::InstanceSlot {
        project: project_name_for(sch_path),
        path: format!("/{}", crate::tools::ensure_root_uuid(sch)),
    }]
}

/// One reference per slot. Single-slot sheets keep the caller's reference
/// verbatim and consume nothing, so the common case is untouched; multi-slot
/// sheets allocate from `used`, reserving as they go.
pub(crate) fn refs_for_slots(
    seed_reference: &str,
    slots: &[crate::tools::InstanceSlot],
    used: &mut std::collections::HashSet<String>,
) -> Vec<String> {
    if slots.len() == 1 {
        return vec![seed_reference.to_string()];
    }
    crate::tools::allocate_references(seed_reference, slots.len(), used)
        .unwrap_or_else(|| vec![seed_reference.to_string(); slots.len()])
}

/// Place one symbol into `sch`: embeds the lib_symbols definition, validates
/// the unit, and adds the positioned instance at every slot in `slots`. Does
/// not write the file -- callers own the read/write cycle (single-add and
/// batch-add alike).
///
/// `refs` is parallel to `slots`; `refs[0]` becomes the symbol's `Reference`
/// property, which KiCAD treats as a cache of *one* of its references.
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_one_component(
    sch: &mut cse::Schematic,
    slots: &[crate::tools::InstanceSlot],
    refs: &[String],
    lib_id: &str,
    x: f64,
    y: f64,
    rotation: f64,
    value: Option<&str>,
    unit: u32,
    src: &dyn cse::library::SymbolLibrarySource,
) -> Result<serde_json::Value, CallToolResult> {
    let reference = refs.first().map(String::as_str).unwrap_or("?");
    // Snap to 1.27mm grid
    let (x, y) = snap_point(x, y, 1.27);
    let val_str = value.unwrap_or(lib_id.split(':').next_back().unwrap_or("?"));

    // Embed the library symbol definition
    if !cse::library::ensure_lib_symbol(sch, lib_id, src) {
        return Err(crate::tools::lib_symbol_not_found_error(lib_id, src));
    }

    // Validate the unit against the resolved symbol BEFORE writing anything:
    // eeschema silently renders an out-of-range unit as unit 1 and the
    // netlister mis-assigns its pins (#35).
    let unit_count = cse::library::symbol_unit_count(lib_id, src).unwrap_or(1);
    if unit < 1 || unit > unit_count {
        return Err(CallToolResult::error(format!(
            "Invalid unit {} for '{}': the symbol has {} unit(s) (valid: 1..={}).",
            unit, lib_id, unit_count, unit_count
        )));
    }

    // Build the Symbol struct
    let mut sym = cse::Symbol::new(lib_id, x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = unit;

    // Reference above the component, Value below; Footprint/Datasheet hidden.
    // Power symbols get their Reference hidden too, matching eeschema: a
    // #PWR designator is never shown on the sheet.
    let hide_reference = lib_id.starts_with("power:") || reference.starts_with("#PWR");
    let positioned = crate::tools::positioned_property;
    sym.properties.push(positioned(
        "Reference",
        reference,
        x,
        y - 3.81,
        0.0,
        hide_reference,
    ));
    sym.properties
        .push(positioned("Value", val_str, x, y + 3.81, 0.0, false));
    sym.properties
        .push(positioned("Footprint", "", x, y, 0.0, true));
    sym.properties
        .push(positioned("Datasheet", "", x, y, 0.0, true));

    // One instance entry per slot, as eeschema writes them:
    // (instances (project "<name>" (path "<path>" (reference ...) (unit 1))))
    // A sheet instantiated four times gets four, or the symbol resolves in
    // none of them and never reaches the netlist.
    for (slot, r) in slots.iter().zip(refs) {
        sym.set_instance_path(&slot.project, &slot.path, r, unit);
    }

    let uuid = sym.uuid.clone();
    sch.add_symbol(sym);

    Ok(json!({
        "added": lib_id,
        "reference": reference,
        "value": val_str,
        "x": x, "y": y,
        "unit": unit,
        "uuid": uuid,
        "shared_instances": slots.len(),
        "references": slots
            .iter()
            .zip(refs)
            .map(|(s, r)| json!({
                "project": s.project,
                "path": s.path,
                "reference": r,
            }))
            .collect::<Vec<_>>()
    }))
}

async fn handle_delete_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.remove_by_reference(&reference) {
        Some(_) => {
            sch.overwrite()?;
            Ok(CallToolResult::json(&json!({ "deleted": reference })))
        }
        None => Ok(CallToolResult::error(format!(
            "Component '{}' not found in schematic",
            reference
        ))),
    }
}

/// Properties this tool exposes as first-class parameters. Routing one of them
/// through `fields` too would let a single call set the same property twice
/// with different values, and for Reference it would skip the instances-path
/// rewrite entirely — a rename that the netlist ignores (#157).
fn is_reserved_property(name: &str) -> bool {
    matches!(name, "Reference" | "Value" | "Footprint" | "Datasheet")
}

/// Does `reference`'s symbol block already carry a `name` property?
fn property_exists(content: &str, reference: &str, name: &str) -> bool {
    find_symbol_instance_block(content, reference).is_some_and(|(start, end)| {
        content[start..end].contains(&format!(r#"(property "{name}" ""#))
    })
}

/// Append a new `(property …)` to `reference`'s symbol block.
///
/// Anchored at the symbol's own `(at …)` and written hidden: a custom field is
/// data, not something to draw over the sheet, and KiCad 10's canonical
/// instance form puts `(hide yes)` as a sibling before `(effects …)` (#96).
/// The `(at …)` is mandatory — a property written without one is defaulted to
/// the sheet origin, which is how every `#PWR` reference once piled up in the
/// top-left corner (#95).
fn append_property(
    content: &str,
    reference: &str,
    name: &str,
    value: &str,
) -> Result<String, String> {
    let (start, end) = find_symbol_instance_block(content, reference)
        .ok_or_else(|| format!("symbol '{reference}' not found in this schematic"))?;
    let block = &content[start..end];

    // The symbol's placement, to anchor the new property on.
    let (x, y) = block
        .find("(at ")
        .and_then(|at| {
            let rest = &block[at + 4..];
            let close = rest.find(')')?;
            let mut parts = rest[..close].split_whitespace();
            Some((
                parts.next()?.parse::<f64>().ok()?,
                parts.next()?.parse::<f64>().ok()?,
            ))
        })
        .ok_or_else(|| format!("'{reference}' has no readable (at …) placement"))?;

    // Match the block's own indentation rather than assuming: eeschema saves
    // with tabs, this crate's writer uses two spaces.
    let indent = block
        .find("(property ")
        .map(|p| {
            let line_start = block[..p].rfind('\n').map_or(0, |n| n + 1);
            block[line_start..p].to_string()
        })
        .unwrap_or_else(|| "\t\t".to_string());

    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    let prop = format!(
        "\n{indent}(property \"{name}\" \"{escaped}\"\n{indent}\t(at {x} {y} 0)\n\
         {indent}\t(hide yes)\n{indent}\t(effects\n{indent}\t\t(font\n{indent}\t\t\t\
         (size 1.27 1.27)\n{indent}\t\t)\n{indent}\t)\n{indent})"
    );

    // Insert before the block's closing paren so the property stays inside it.
    let close = content[..end]
        .rfind(')')
        .ok_or_else(|| format!("symbol block for '{reference}' is malformed"))?;
    Ok(format!(
        "{}{}{}",
        &content[..close],
        prop,
        &content[close..]
    ))
}

/// Rewrite the `(reference "…")` inside every unit's `(instances …)` block.
///
/// Returns the updated content and how many were rewritten. A multi-unit part
/// is placed once per unit and each placement carries its own instances block,
/// so a rename has to reach all of them or the units disagree about their own
/// designator.
fn rewrite_instance_references(
    content: &str,
    old_ref: &str,
    new_ref: &str,
) -> Result<(String, usize), String> {
    let blocks = find_all_symbol_instance_blocks(content, new_ref);
    if blocks.is_empty() {
        return Err(format!("symbol '{old_ref}' not found after the rename"));
    }

    let search = format!(r#"(reference "{old_ref}")"#);
    let replacement = format!(r#"(reference "{new_ref}")"#);
    let mut edits = Vec::new();
    for (start, end) in &blocks {
        let block = &content[*start..*end];
        let mut from = 0usize;
        while let Some(rel) = block[from..].find(&search) {
            let at = *start + from + rel;
            edits.push(SexpEdit::replace(
                at,
                at + search.len(),
                replacement.clone(),
            ));
            from += rel + search.len();
        }
    }
    if edits.is_empty() {
        return Err(format!(
            "'{new_ref}' has no (reference \"{old_ref}\") in its instances path — \
             the property was renamed but the netlist still reads the old designator"
        ));
    }
    let count = edits.len();
    Ok((apply_edits(content.to_string(), edits), count))
}

async fn handle_edit_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let mut content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut changed = Vec::new();

    // Helper: update a property field value in-place within the symbol block
    // for `ref_`. Returns the reason on failure so the caller can report it
    // instead of silently claiming success.
    let update_field =
        |content: &str, ref_: &str, field: &str, new_val: &str| -> Result<String, String> {
            let (sym_start, sym_end) = find_symbol_instance_block(content, ref_)
                .ok_or_else(|| format!("symbol '{ref_}' not found in this schematic"))?;
            let sym_block = &content[sym_start..sym_end];
            let field_search = format!(r#"(property "{field}" ""#);
            let field_offset = sym_block
                .find(&field_search)
                .map(|o| sym_start + o + field_search.len())
                .ok_or_else(|| format!("'{ref_}' has no '{field}' property"))?;
            // Find the closing quote of the current value
            let val_end = content[field_offset..]
                .find('"')
                .map(|o| field_offset + o)
                .ok_or_else(|| format!("'{field}' property on '{ref_}' is malformed"))?;
            Ok(format!(
                "{}{}{}",
                &content[..field_offset],
                new_val,
                &content[val_end..]
            ))
        };

    let mut errors: Vec<String> = Vec::new();
    // A macro rather than a closure: the body also needs `changed`/`errors`
    // between calls (the instances rewrite below, and the custom-field loop),
    // and a closure capturing them mutably would lock both for its lifetime.
    macro_rules! apply {
        ($field:expr, $new_val:expr) => {
            match update_field(&content, &reference, $field, $new_val) {
                Ok(updated) => {
                    content = updated;
                    changed.push(format!("{} → {}", $field, $new_val));
                }
                Err(why) => errors.push(format!("{}: {}", $field, why)),
            }
        };
    }

    if let Some(new_ref) = opt_str(args, "new_reference") {
        apply!("Reference", new_ref);
        // A designator lives in TWO places. The (property "Reference" …) is
        // what renders; the (reference …) inside (instances …) is what KiCad
        // reads when it builds the netlist. Rewriting only the property leaves
        // the netlist on the old designator, so the rename appears to work in
        // eeschema and is ignored everywhere it matters (#157).
        match rewrite_instance_references(&content, &reference, new_ref) {
            Ok((updated, count)) => {
                content = updated;
                changed.push(format!("instances reference → {new_ref} ({count})"));
            }
            Err(why) => errors.push(format!("instances reference: {why}")),
        }
    }
    if let Some(val) = opt_str(args, "value") {
        apply!("Value", val);
    }
    if let Some(fp) = opt_str(args, "footprint") {
        apply!("Footprint", fp);
    }
    if let Some(ds) = opt_str(args, "datasheet") {
        apply!("Datasheet", ds);
    }

    // `fields` has been in this tool's schema since it shipped and the handler
    // never read it, so custom properties were dropped and the call still
    // reported success (#158). An existing property is updated in place; a new
    // one is appended to the symbol block.
    let custom_fields = args["fields"].as_object();
    if let Some(fields) = custom_fields {
        for (name, value) in fields {
            let Some(value) = value.as_str() else {
                errors.push(format!("{name}: field values must be strings"));
                continue;
            };
            if is_reserved_property(name) {
                errors.push(format!(
                    "{name}: set this through the '{}' parameter, not 'fields'",
                    name.to_ascii_lowercase()
                ));
                continue;
            }
            if property_exists(&content, &reference, name) {
                apply!(name.as_str(), value);
            } else {
                match append_property(&content, &reference, name, value) {
                    Ok(updated) => {
                        content = updated;
                        changed.push(format!("{name} → {value} (added)"));
                    }
                    Err(why) => errors.push(format!("{name}: {why}")),
                }
            }
        }
    }

    // A request that changed nothing is a failure, not a success — silently
    // reporting `"changes": []` is what let the tab-indentation bug hide, and
    // what made a fields-only call report success while dropping every field
    // (#158): with `fields` unread, both `changed` and `errors` came back
    // empty and this guard never fired.
    if changed.is_empty() && custom_fields.is_some_and(|f| !f.is_empty()) && errors.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No fields were updated on '{reference}'"
        )));
    }
    if changed.is_empty() && !errors.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No fields were updated on '{}': {}",
            reference,
            errors.join("; ")
        )));
    }

    if !changed.is_empty() {
        let item_id = symbol_item_id(&expected, &reference)?;
        let command = SchematicCommand::replace_item_from_document(
            &expected,
            &content,
            item_id,
            format!("Edit {reference}"),
        )?;
        commit_command(&sch_path, &command)?;
    }

    let mut result = json!({
        "reference": reference,
        "changes": changed
    });
    if !errors.is_empty() {
        result["errors"] = json!(errors);
    }
    Ok(CallToolResult::json(&result))
}

async fn handle_get_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference(&reference) {
        Some(sym) => {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            Ok(CallToolResult::json(&json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y'),
                "uuid": sym.uuid
            })))
        }
        None => Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        ))),
    }
}

async fn handle_list_schematic_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;

    let items: Vec<serde_json::Value> = sch
        .symbols
        .iter()
        .map(|sym| {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y')
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "components": items
    })))
}

async fn handle_move_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let new_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (new_x, new_y) = snap_point(new_x, new_y, 1.27);

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.move_to(new_x, new_y);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "moved": reference, "x": new_x, "y": new_y }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

async fn handle_rotate_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.set_rotation(rotation);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "rotated": reference, "rotation": rotation }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

/// One wire as its two endpoints, in schematic millimetres.
type WireSegment = ((f64, f64), (f64, f64));

/// Whether some wire runs *through* `(x, y)` rather than starting or ending
/// there.
///
/// This is what separates a junction that may be dragged along with a moving
/// pin from one that may not. If every wire at the point merely ends there,
/// they all follow the pin and the dot should follow too. If one passes
/// through, that wire is staying put and the dot is holding a T together with
/// it — moving the dot would break the T silently.
fn wire_passes_through(segs: &[WireSegment], x: f64, y: f64, tol: f64) -> bool {
    segs.iter().any(|(s, e)| {
        point_on_segment(x, y, s.0, s.1, e.0, e.1, tol)
            && !points_coincident(x, y, s.0, s.1, tol)
            && !points_coincident(x, y, e.0, e.1, tol)
    })
}

/// Move a symbol and drag everything attached to its pins along with it.
///
/// A plain move relocates the symbol and leaves its connections where they
/// were, silently disconnecting it — the pins are simply somewhere else now.
/// This finds every item sitting exactly on one of the symbol's pin endpoints
/// and shifts it by the same delta, so wires stretch from their far end and
/// labels, no-connects and junctions travel with the pin they belong to.
///
/// Deliberately not moved: anything merely *near* a pin. Attachment in KiCAD is
/// exact coincidence, so the test is exact coincidence too.
///
/// A junction is the one item that can be wrong to move. Where a wire passes
/// *through* the pin point rather than ending there, the junction belongs to
/// that through-wire as much as to the pin, and dragging it away would break
/// the T. Those are left in place and reported rather than guessed at.
async fn handle_move_connected(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    const TOL: f64 = 0.01;

    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let want_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let want_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (want_x, want_y) = snap_point(want_x, want_y, 1.27);

    // Pin endpoints as they stand. Read from the s-expression tree because pin
    // geometry lives in lib_symbols, which cse keeps as opaque raw nodes.
    let (_src, tree) = read_schematic(std::path::Path::new(&sch_path))?;
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut old_pins: Vec<(f64, f64)> = Vec::new();
    for inst in extract_symbol_instances(&tree) {
        if inst.reference != reference {
            continue;
        }
        // Every unit of a multi-unit part, since all of them move together.
        if let Some(sym) = find_lib_symbol(&lib_syms, &inst) {
            let t = inst.pin_transform();
            for p in extract_lib_pins_for_unit(sym, inst.unit) {
                old_pins.push(pin_endpoint(&p, t));
            }
        }
    }

    let mut sch = cse::Schematic::load(&sch_path)?;

    // The delta the placement actually took — grid snapping can make it differ
    // from the requested one, and dragging by the requested delta would leave
    // everything a fraction of a millimetre off its pin.
    let (dx, dy) = match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            let (ox, oy) = sym.position();
            sym.move_to(want_x, want_y);
            let (nx, ny) = sym.position();
            (nx - ox, ny - oy)
        }
        None => return Err(anyhow::anyhow!("Component '{}' not found", reference)),
    };

    let on_pin = |x: f64, y: f64| {
        old_pins
            .iter()
            .any(|&(px, py)| points_coincident(x, y, px, py, TOL))
    };

    // Snapshot wire geometry before mutating, for the junction pass-through test.
    let orig: Vec<WireSegment> = sch.wires.iter().map(|w| (w.start, w.end)).collect();

    let mut wire_ends_moved = 0usize;
    for w in sch.wires.iter_mut() {
        if on_pin(w.start.0, w.start.1) {
            w.start = (w.start.0 + dx, w.start.1 + dy);
            wire_ends_moved += 1;
        }
        if on_pin(w.end.0, w.end.1) {
            w.end = (w.end.0 + dx, w.end.1 + dy);
            wire_ends_moved += 1;
        }
    }

    let mut labels_moved = 0usize;
    macro_rules! drag_labels {
        ($coll:expr) => {
            for l in $coll.iter_mut() {
                if on_pin(l.at.x, l.at.y) {
                    l.at.x += dx;
                    l.at.y += dy;
                    labels_moved += 1;
                }
            }
        };
    }
    drag_labels!(sch.labels);
    drag_labels!(sch.global_labels);
    drag_labels!(sch.hierarchical_labels);

    let mut no_connects_moved = 0usize;
    for nc in sch.no_connects.iter_mut() {
        if on_pin(nc.x, nc.y) {
            nc.x += dx;
            nc.y += dy;
            no_connects_moved += 1;
        }
    }

    let mut junctions_moved = 0usize;
    let mut junctions_left: Vec<serde_json::Value> = Vec::new();
    for j in sch.junctions.iter_mut() {
        if !on_pin(j.x, j.y) {
            continue;
        }
        if wire_passes_through(&orig, j.x, j.y, TOL) {
            junctions_left.push(json!({ "x": j.x, "y": j.y,
                "reason": "a wire passes through this point; the junction holds that T together" }));
        } else {
            j.x += dx;
            j.y += dy;
            junctions_moved += 1;
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "moved": reference,
        "x": want_x, "y": want_y,
        "dx": dx, "dy": dy,
        "pins": old_pins.len(),
        "wire_endpoints_moved": wire_ends_moved,
        "labels_moved": labels_moved,
        "no_connects_moved": no_connects_moved,
        "junctions_moved": junctions_moved,
        "junctions_left_behind": junctions_left,
    })))
}

async fn handle_move_region(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    // Collect references of symbols within the bounding box
    let refs_to_move: Vec<String> = sch
        .symbols
        .within_rectangle(x1, y1, x2, y2)
        .iter()
        .filter_map(|s| s.reference().map(String::from))
        .collect();

    let mut moved = Vec::new();
    for reference in &refs_to_move {
        if let Some(sym) = sch.symbols.by_reference_mut(reference) {
            let (ox, oy) = sym.position();
            let (nx, ny) = snap_point(ox + dx, oy + dy, 1.27);
            sym.move_to(nx, ny);
            moved.push(reference.clone());
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved
    })))
}

async fn handle_annotate_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    crate::tools::cli::annotate_schematic(&ctx.config.kicad_cli, &sch_path).await?;
    Ok(CallToolResult::text("Annotation complete."))
}

async fn handle_get_schematic_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let inst = match instances.iter().find(|i| i.reference == reference) {
        Some(i) => i,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the library symbol definition within the schematic's lib_symbols section
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let lib_sym = find_lib_symbol(&lib_syms, inst);

    // A missing embedded definition is an error, not an empty pin list —
    // silently returning [] hid every bad-lib_id component until wiring or
    // netlisting failed much later (#34).
    let Some(sym) = lib_sym else {
        return Ok(CallToolResult::error(format!(
            "Component '{}' has no embedded definition for '{}' in this \
             schematic's lib_symbols — it was likely added with a lib_id that \
             doesn't exist in the installed libraries, so it is invisible to \
             KiCAD's netlister. Re-add it with a valid lib_id \
             (delete_schematic_component + add_schematic_component).",
            reference,
            inst.lib_symbol_name()
        )));
    };
    // Unit-aware: only this instance's unit (plus _0_1 commons), not every
    // unit's pins superimposed (#35).
    let lib_pins = extract_lib_pins_for_unit(sym, inst.unit);
    // A definition that resolves but has ZERO pins is almost always an
    // `(extends "Parent")` stub — kicad-cli can't resolve those either (the
    // netlist shows a pinless part), so silent pins:[] hides real breakage.
    // The #34 guard above only catches MISSING definitions.
    if lib_pins.is_empty() {
        if let Some(parent) = sym.find_str("extends") {
            return Ok(CallToolResult::error(format!(
                "Component '{}': the embedded definition for '{}' is an \
                 (extends \"{}\") stub with no pins of its own. kicad-cli \
                 cannot resolve extends stubs (the netlist gets a pinless \
                 part). Re-add the component (delete_schematic_component + \
                 add_schematic_component) so the definition is embedded in \
                 full, or place the parent symbol '{}' directly.",
                reference,
                inst.lib_symbol_name(),
                parent,
                parent
            )));
        }
    }
    let t = inst.pin_transform();
    let pins: Vec<serde_json::Value> = lib_pins
        .iter()
        .map(|p| {
            let (sx, sy) = pin_endpoint(p, t);
            json!({
                "number": p.number,
                "name": p.name,
                "x": sx,
                "y": sy,
                // Which way the pin faces away from the body (0 = east). A
                // label here should read that way, or it runs back over the
                // symbol's pin names.
                "orientation_degrees": pin_outward_direction(p, t),
                "length_mm": p.length
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "component_x": inst.x,
        "component_y": inst.y,
        "rotation": inst.rotation,
        "pins": pins
    })))
}

async fn handle_batch_get_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let (_, tree) = read_schematic(&sch_path)?; // single read
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let results: Vec<serde_json::Value> = refs
        .iter()
        .map(|reference| {
            let inst = match instances.iter().find(|i| &i.reference == reference) {
                Some(i) => i,
                None => return json!({ "reference": reference, "error": "not found" }),
            };
            let lib_sym = find_lib_symbol(&lib_syms, inst);
            // Per-entry error rather than a silent empty pin list (#34).
            let Some(sym) = lib_sym else {
                return json!({
                    "reference": reference,
                    "error": format!(
                        "no embedded definition for '{}' in lib_symbols — \
                         likely added with a nonexistent lib_id",
                        inst.lib_symbol_name()
                    )
                });
            };
            let lib_pins = extract_lib_pins_for_unit(sym, inst.unit);
            // Zero pins from a resolving definition = extends stub (#35);
            // mirror the single-component handler's structured error.
            if lib_pins.is_empty() {
                if let Some(parent) = sym.find_str("extends") {
                    return json!({
                        "reference": reference,
                        "error": format!(
                            "embedded definition for '{}' is an (extends \"{}\") \
                             stub with no pins — re-add the component so it is \
                             embedded in full",
                            inst.lib_symbol_name(), parent
                        )
                    });
                }
            }
            let t = inst.pin_transform();
            let pins: Vec<serde_json::Value> = lib_pins
                .iter()
                .map(|p| {
                    let (sx, sy) = pin_endpoint(p, t);
                    json!({
                        "number": p.number,
                        "name": p.name,
                        "x": sx,
                        "y": sy,
                        "orientation_degrees": pin_outward_direction(p, t),
                        "length_mm": p.length
                    })
                })
                .collect();
            json!({ "reference": reference, "x": inst.x, "y": inst.y, "pins": pins })
        })
        .collect();

    Ok(CallToolResult::json(&json!({ "components": results })))
}

async fn handle_get_schematic_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tmp_dir = std::env::temp_dir().join(format!("konnect_{}", new_uuid()));
    tokio::fs::create_dir_all(&tmp_dir).await?;

    // KiCAD 10 CLI only supports SVG export for schematics (no bitmap)
    let svg_path =
        crate::tools::cli::render_schematic_svg(&ctx.config.kicad_cli, &sch_path, &tmp_dir).await?;

    let svg_content = tokio::fs::read_to_string(&svg_path).await?;
    tokio::fs::remove_dir_all(&tmp_dir).await.ok();

    // Return as text content (SVG is XML text, not a raster image)
    Ok(crate::mcp::protocol::CallToolResult {
        content: vec![crate::mcp::protocol::ToolContent::Text {
            text: format!("SVG schematic rendered. {} bytes.\n\nNote: KiCAD 10 CLI exports schematics as SVG only (no bitmap). \
                          The SVG file has been generated. Use export_schematic_pdf for a PDF version.", svg_content.len()),
        }],
        is_error: false,
    })
}

async fn handle_add_component_annotation(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let key = match require_str(args, "key") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match require_str(args, "value") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = read_consistent(&sch_path)?;
    let expected = content.clone();

    // Find the symbol block for this reference
    let (sym_start, sym_end) = match find_symbol_instance_block(&content, &reference) {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the position just before (instances in the symbol block, or before closing paren
    let sym_block = &content[sym_start..sym_end];
    let insert_rel = sym_block
        .find("(instances")
        .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
    let insert_abs = sym_start + insert_rel;

    // Build the property S-expression
    let prop_sexp = format!(
        "    (property \"{key}\" \"{value}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
    );

    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
    let item_id = symbol_item_id(&expected, &reference)?;
    let command = SchematicCommand::replace_item_from_document(
        &expected,
        &new_content,
        item_id,
        format!("Add {key} property to {reference}"),
    )?;
    commit_command(&sch_path, &command)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "added_property": key,
        "value": value
    })))
}

fn symbol_item_id(content: &str, reference: &str) -> anyhow::Result<ItemId> {
    let (start, end) = find_symbol_instance_block(content, reference)
        .ok_or_else(|| anyhow::anyhow!("component '{reference}' not found"))?;
    let symbol = parse_sexp(&content[start..end])?;
    let uuid = symbol
        .find_str("uuid")
        .ok_or_else(|| anyhow::anyhow!("component '{reference}' has no UUID"))?;
    Ok(ItemId::new(uuid.to_owned())?)
}

async fn handle_group_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let group_name = match require_str(args, "group_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if refs.is_empty() {
        return Ok(CallToolResult::error("No references provided"));
    }

    let mut content = read_consistent(&sch_path)?;
    let expected = content.clone();
    let mut grouped = Vec::new();
    let mut item_ids = Vec::new();

    for reference in &refs {
        let (sym_start, sym_end) = match find_symbol_instance_block(&content, reference) {
            Some(r) => r,
            None => continue,
        };

        let sym_block = &content[sym_start..sym_end];
        let insert_rel = sym_block
            .find("(instances")
            .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
        let insert_abs = sym_start + insert_rel;

        let prop_sexp = format!(
            "    (property \"Group\" \"{group_name}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
        );

        content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
        item_ids.push(symbol_item_id(&expected, reference)?);
        grouped.push(reference.clone());
    }

    if !item_ids.is_empty() {
        let command = SchematicCommand::replace_items_from_document(
            &expected,
            &content,
            item_ids,
            format!("Group components as {group_name}"),
        )?;
        commit_command(&sch_path, &command)?;
    }

    Ok(CallToolResult::json(&json!({
        "group_name": group_name,
        "grouped_count": grouped.len(),
        "grouped": grouped
    })))
}

async fn handle_replace_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_lib_id = match require_str(args, "new_lib_id") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let new_unit = opt_f64(args, "unit").map(|u| u as u32);

    let mut content = read_consistent(&sch_path)?;
    let expected = content.clone();

    // Find the symbol block for this reference, resolving through (instances)
    // as well as the cached Reference property — in a sheet instantiated more
    // than once the requested reference usually exists only there.
    let matches = super::find_symbol_blocks_by_any_reference(&content, &reference);
    let (sym_start, sym_end) = match matches.first() {
        Some(&r) => r,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Every other reference this one symbol answers to. Replacing it changes
    // all of them at once, which is correct — they are one placement in a
    // shared sheet — but a caller who asked to swap "J3" is entitled to know
    // that J2, J4 and J7 changed with it.
    let mut also_affects: Vec<super::InstanceRef> =
        super::symbol_instance_refs(&content, (sym_start, sym_end))
            .into_iter()
            .filter(|r| r.reference != reference)
            .collect();
    also_affects.sort_by(|a, b| {
        a.project
            .cmp(&b.project)
            .then_with(|| a.reference.cmp(&b.reference))
    });
    also_affects.dedup_by(|a, b| a.reference == b.reference && a.project == b.project);

    // Find the (lib_id "OLD") and replace it — searching only within this
    // symbol's block, so a malformed instance can't reach into the next one.
    let sym_block = &content[sym_start..sym_end];
    let lib_id_pat = "(lib_id \"";
    let lib_id_rel = match sym_block.find(lib_id_pat) {
        Some(o) => o,
        None => {
            return Ok(CallToolResult::error(
                "Could not find lib_id in symbol block",
            ))
        }
    };
    let lib_id_abs = sym_start + lib_id_rel + lib_id_pat.len();
    let lib_id_end = match content[lib_id_abs..].find('"') {
        Some(o) => lib_id_abs + o,
        None => return Ok(CallToolResult::error("Malformed lib_id")),
    };

    let old_lib_id = content[lib_id_abs..lib_id_end].to_string();

    let new_content = apply_edits(
        content,
        vec![SexpEdit::replace(
            lib_id_abs,
            lib_id_end,
            new_lib_id.clone(),
        )],
    );
    content = new_content;

    // Optional unit change, validated against the NEW symbol's unit count
    // (#35). Applied before the embed so all edits land in one write.
    let src = crate::tools::library::KiCadSymbolSource::for_file(&sch_path);
    if let Some(unit) = new_unit {
        let unit_count = cse::library::symbol_unit_count(&new_lib_id, &src).unwrap_or(1);
        if unit < 1 || unit > unit_count {
            return Ok(CallToolResult::error(format!(
                "Invalid unit {} for '{}': the symbol has {} unit(s) (valid: 1..={}).",
                unit, new_lib_id, unit_count, unit_count
            )));
        }
        // Re-find the block (offsets moved with the lib_id edit), then update
        // every `(unit N)` inside it — the symbol's own and the one in each of
        // its (instances …) entries. They all describe the same placement, so
        // they must move together.
        if let Some(&(s, e)) =
            super::find_symbol_blocks_by_any_reference(&content, &reference).first()
        {
            let block = &content[s..e];
            let mut edits = Vec::new();
            let mut from = 0usize;
            while let Some(rel) = block[from..].find("(unit ") {
                let num_start = from + rel + "(unit ".len();
                let Some(close) = block[num_start..].find(')') else {
                    break;
                };
                edits.push(SexpEdit::replace(
                    s + num_start,
                    s + num_start + close,
                    unit.to_string(),
                ));
                from = num_start + close;
            }
            content = apply_edits(content, edits);
        }
    }

    // Ensure the new library symbol definition is present. Bail BEFORE writing:
    // a replace that can't embed its definition would leave the component
    // netlist-invisible (#34).
    if !super::ensure_lib_symbol_in_schematic(&mut content, &new_lib_id, &src) {
        return Ok(crate::tools::lib_symbol_not_found_error(&new_lib_id, &src));
    }
    write_atomic_if_unchanged(&sch_path, &expected, &content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "old_lib_id": old_lib_id,
        "new_lib_id": new_lib_id,
        "unit": new_unit,
        // Non-empty means the sheet is instantiated more than once and this
        // swap changed every copy. Not a warning — it is what a shared sheet
        // means — but silence here would be misleading.
        "also_affects": also_affects,
        "shared_instances": also_affects.len() + 1,
    })))
}

// Library symbol resolution moved to tools/mod.rs (shared with sch_wiring.rs)

// `stub_symbol_dir` returns a MutexGuard that the async tests then hold across
// their `.await`s, which is what `await_holding_lock` warns about. It is
// deliberate and safe here: the lock serialises process-wide `KICAD*_DIR`
// environment variables, which the awaited calls read, so releasing it early
// would defeat its only purpose. cargo runs each test on its own OS thread with
// its own current-thread runtime, and each runtime drives exactly one task, so
// there is no second task that could contend for the guard and deadlock.
#[allow(clippy::await_holding_lock)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// Serializes tests that set KICAD10_SYMBOL_DIR (process-wide env), shared
    /// with every other module that does so.
    use crate::tools::KICAD_ENV_LOCK as SYMBOL_DIR_ENV;

    /// Only the stub carries this, so asserting on it proves a placement
    /// resolved the fixture and not a KiCad library installed on the machine.
    const STUB_MARKER: &str = "stub://device";

    /// A stub symbol library so component adds resolve without an installed
    /// KiCad (CI has none): Device:R and Device:C_Polarized in the KiCad 10
    /// symdir layout, plus a `sym-lib-table` registering them.
    ///
    /// The returned tempdir doubles as the project directory — put the test's
    /// schematic in it, so the project table is the one consulted.
    fn stub_symbol_dir() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = SYMBOL_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let symdir = dir.path().join("Device.kicad_symdir");
        std::fs::create_dir_all(&symdir).unwrap();
        let symbol = |name: &str| {
            format!(
                "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"{name}\"\n\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t(property \"Value\" \"{name}\" (at 0 0 0))\n\t\t(property \"Datasheet\" \"{STUB_MARKER}\" (at 0 0 0))\n\t\t(symbol \"{name}_0_1\"\n\t\t\t(pin passive line (at 0 3.81 270) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t\t(pin passive line (at 0 -3.81 90) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"2\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t)\n)\n"
            )
        };
        std::fs::write(symdir.join("R.kicad_sym"), symbol("R")).unwrap();
        std::fs::write(symdir.join("C_Polarized.kicad_sym"), symbol("C_Polarized")).unwrap();
        // LM2904-style multi-unit part: unit 1 = pins 1-3, unit 2 = pins 5-7,
        // unit 3 = power pins 4/8 (#35 repro shape).
        let pin = |num: &str, x: f64, y: f64, angle: u32| {
            format!(
                "\t\t\t(pin passive line (at {x} {y} {angle}) (length 2.54)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"{num}\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n"
            )
        };
        let opamp = format!(
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"OPAMP_DUAL\"\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"OPAMP_DUAL\" (at 0 0 0))\n\t\t(symbol \"OPAMP_DUAL_1_1\"\n{}{}{}\t\t)\n\t\t(symbol \"OPAMP_DUAL_2_1\"\n{}{}{}\t\t)\n\t\t(symbol \"OPAMP_DUAL_3_1\"\n{}{}\t\t)\n\t)\n)\n",
            pin("1", -7.62, 2.54, 0),
            pin("2", -7.62, -2.54, 0),
            pin("3", 7.62, 0.0, 180),
            pin("5", -7.62, 2.54, 0),
            pin("6", -7.62, -2.54, 0),
            pin("7", 7.62, 0.0, 180),
            pin("4", 0.0, -7.62, 90),
            pin("8", 0.0, 7.62, 270),
        );
        std::fs::write(symdir.join("OPAMP_DUAL.kicad_sym"), opamp).unwrap();
        // Derived symbol: an extends stub with no drawing of its own, like
        // Amplifier_Operational:NE5532 → LM2904.
        std::fs::write(
            symdir.join("OPAMP_DERIVED.kicad_sym"),
            "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"OPAMP_DERIVED\"\n\t\t(extends \"OPAMP_DUAL\")\n\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t(property \"Value\" \"OPAMP_DERIVED\" (at 0 0 0))\n\t)\n)\n",
        )
        .unwrap();
        // A project sym-lib-table, checked before the global one, is what
        // makes this hermetic: KICAD10_SYMBOL_DIR alone is not enough, because
        // the global table's own `Device` entry resolves to whatever KiCad the
        // developer has installed and would shadow the stub.
        std::fs::write(
            dir.path().join("sym-lib-table"),
            format!(
                "(sym_lib_table\n  (version 7)\n  (lib (name \"Device\") (type \"KiCad\") (uri \"{}\") (options \"\") (descr \"\"))\n)\n",
                symdir.display()
            ),
        )
        .unwrap();
        std::env::set_var("KICAD10_SYMBOL_DIR", dir.path());
        (dir, guard)
    }

    #[tokio::test]
    async fn create_schematic_writes_root_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.kicad_sch");
        let ctx = test_ctx();

        let result = handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(
            sch.uuid.is_some(),
            "root (uuid ...) is required for KiCAD's netlister to resolve instance paths"
        );
    }

    #[tokio::test]
    async fn add_component_writes_eeschema_style_instance_path() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("amp.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        // Guards the fixture itself: the project sym-lib-table must win over
        // any real Device library the developer has installed, or these tests
        // silently stop exercising the stub they set up.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(STUB_MARKER),
            "Device:R must resolve from the stub, not an installed KiCad library"
        );

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("root uuid present");
        let sym = sch.symbols.by_reference("R1").unwrap();
        // KiCAD only forms wire-only nets when the instance path is exactly
        // "/<root-uuid>"; the project key mirrors eeschema (file stem).
        assert!(
            sym.has_instance_path("amp", &format!("/{}", root_uuid)),
            "instance path must be /<root-uuid> under the file-stem project name"
        );
        // A single-instance sheet takes the caller's reference verbatim and
        // gains no extra paths — the multi-instance work must not touch it.
        assert_eq!(body(&result)["shared_instances"], json!(1));
    }

    /// The JSON body of a successful tool result.
    fn body(result: &CallToolResult) -> serde_json::Value {
        let text = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected a text content block"),
        };
        serde_json::from_str(&text).expect("result body is JSON")
    }

    /// A sub-sheet already instantiated three times in one project and once in
    /// another, shaped like `ModuleBase/Interface.kicad_sch`. The existing
    /// symbol is R4/R5/R6 in ModuleBase and R25 in a stale PowerModule set.
    fn shared_subsheet(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("Interface.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(uuid \"a44ef46f\")\n\
\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 10 10 0)\n\t\t(unit 1)\n\
\t\t(property \"Reference\" \"R4\"\n\t\t\t(at 10 6 0)\n\t\t)\n\
\t\t(instances\n\
\t\t\t(project \"ModuleBase\"\n\
\t\t\t\t(path \"/root/if1\"\n\t\t\t\t\t(reference \"R4\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\
\t\t\t\t(path \"/root/if2\"\n\t\t\t\t\t(reference \"R5\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\
\t\t\t\t(path \"/root/if3\"\n\t\t\t\t\t(reference \"R6\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\
\t\t\t(project \"PowerModule\"\n\
\t\t\t\t(path \"/pm/if1\"\n\t\t\t\t\t(reference \"R25\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn adding_to_a_shared_sheet_writes_every_instance_path() {
        let (dir, _env) = stub_symbol_dir();
        let path = shared_subsheet(dir.path());
        let ctx = test_ctx();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 40.0, "y": 40.0,
                "value": "100k",
                "reference": "R7"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let v = body(&result);
        assert_eq!(
            v["shared_instances"],
            json!(4),
            "three ModuleBase instantiations plus the stale PowerModule one"
        );

        let sch = cse::Schematic::load(&path).unwrap();
        let placed = sch
            .symbols
            .iter()
            .find(|s| s.value_str() == Some("100k"))
            .expect("the added resistor");

        // The defect this closes: one path here left the symbol unannotated in
        // ModuleBase and absent from its netlist.
        for p in ["/root/if1", "/root/if2", "/root/if3"] {
            assert!(
                placed.has_instance_path("ModuleBase", p),
                "missing ModuleBase path {p}"
            );
        }
        assert!(placed.has_instance_path("PowerModule", "/pm/if1"));

        // One designator per instantiation, none of them colliding with the
        // R4/R5/R6/R25 already in the file.
        let refs: Vec<&str> = v["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["reference"].as_str().unwrap())
            .collect();
        assert_eq!(refs.len(), 4);
        for r in &refs {
            assert!(
                !["R4", "R5", "R6", "R25"].contains(r),
                "{r} collides with an existing designator"
            );
        }
        let unique: std::collections::HashSet<_> = refs.iter().collect();
        assert_eq!(unique.len(), 4, "designators must be distinct: {refs:?}");
    }

    #[tokio::test]
    async fn an_unannotated_add_to_a_shared_sheet_stays_unannotated() {
        let (dir, _env) = stub_symbol_dir();
        let path = shared_subsheet(dir.path());
        let ctx = test_ctx();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 40.0, "y": 40.0,
                "value": "100k"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        // Still one entry per instantiation — the paths are what the netlist
        // needs — but no invented designators.
        let v = body(&result);
        assert_eq!(v["shared_instances"], json!(4));
        for e in v["references"].as_array().unwrap() {
            assert_eq!(e["reference"], json!("?"));
        }
    }

    #[tokio::test]
    async fn add_component_writes_requested_unit() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("multi.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:OPAMP_DUAL",
                "x": 100.0, "y": 80.0,
                "reference": "U1",
                "unit": 3
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "unit 3 of a 3-unit part must be accepted");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch.symbols.by_reference("U1").unwrap();
        assert_eq!(sym.unit, 3, "symbol (unit N) must match the requested unit");
        let root_uuid = sch.uuid.clone().unwrap();
        // Instance entry must carry the same unit, not a hardcoded 1.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(&format!("/{}", root_uuid)));
        assert!(raw.contains("(unit 3)"), "instance unit must be 3");
    }

    fn content_text(res: &CallToolResult) -> String {
        match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_component_rejects_out_of_range_unit() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("units.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        for bad_unit in [0, 99] {
            let result = handle_add_schematic_component(
                &json!({
                    "schematic": path.display().to_string(),
                    "lib_id": "Device:OPAMP_DUAL",
                    "x": 100.0, "y": 80.0,
                    "reference": "U1",
                    "unit": bad_unit
                }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(result.is_error, "unit {bad_unit} must be rejected");
            let text = content_text(&result);
            assert!(
                text.contains("3 unit"),
                "error must state the unit count: {text}"
            );
        }
        // A single-unit symbol only accepts unit 1.
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "R1",
                "unit": 2
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            result.is_error,
            "unit 2 of a 1-unit symbol must be rejected"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "rejected placements must not modify the schematic"
        );
    }

    #[tokio::test]
    async fn pin_locations_are_unit_aware() {
        // The #35 repro: an LM2904-style dual op-amp placed as unit 1 and as
        // unit 2 must report DISJOINT pin sets, not all units superimposed.
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("dual.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        for (reference, unit, x) in [("U1", 1, 100.0), ("U2", 2, 150.0)] {
            let res = handle_add_schematic_component(
                &json!({
                    "schematic": path.display().to_string(),
                    "lib_id": "Device:OPAMP_DUAL",
                    "x": x, "y": 80.0,
                    "reference": reference,
                    "unit": unit
                }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!res.is_error, "placing {reference}: {:?}", res.content);
        }

        let pin_numbers = |res: &CallToolResult| -> Vec<String> {
            let out: serde_json::Value = serde_json::from_str(&content_text(res)).unwrap();
            let mut nums: Vec<String> = out["pins"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["number"].as_str().unwrap().to_string())
                .collect();
            nums.sort();
            nums
        };

        let u1 = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "U1" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!u1.is_error);
        assert_eq!(pin_numbers(&u1), vec!["1", "2", "3"], "unit 1 pins only");

        let u2 = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "U2" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!u2.is_error);
        assert_eq!(pin_numbers(&u2), vec!["5", "6", "7"], "unit 2 pins only");

        // Batch variant agrees.
        let batch = handle_batch_get_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["U1", "U2"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        let out: serde_json::Value = serde_json::from_str(&content_text(&batch)).unwrap();
        let comps = out["components"].as_array().unwrap();
        let nums = |i: usize| -> Vec<String> {
            let mut v: Vec<String> = comps[i]["pins"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["number"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(nums(0), vec!["1", "2", "3"]);
        assert_eq!(nums(1), vec!["5", "6", "7"]);
    }

    #[tokio::test]
    async fn pin_locations_error_on_extends_stub_with_zero_pins() {
        // A pre-flattening schematic: the embedded definition for the derived
        // symbol is an (extends "Parent") stub with no pins. The #34 guard
        // only catches MISSING definitions; a resolving-but-pinless stub must
        // be a structured error too, not pins:[] (#35).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stub.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t\t(symbol \"Device:OPAMP_DERIVED\"\n\t\t\t(extends \"Device:OPAMP_DUAL\")\n\t\t\t(property \"Reference\" \"U\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:OPAMP_DERIVED\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let res = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "U1" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(res.is_error, "extends stub with zero pins must be an error");
        let text = content_text(&res);
        assert!(
            text.contains("Device:OPAMP_DERIVED"),
            "error must name the lib_id: {text}"
        );
        assert!(
            text.contains("Device:OPAMP_DUAL"),
            "error must name the extends target: {text}"
        );

        // Batch variant reports it per-entry.
        let batch = handle_batch_get_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["U1"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        let out: serde_json::Value = serde_json::from_str(&content_text(&batch)).unwrap();
        let err = out["components"][0]["error"].as_str().unwrap_or("");
        assert!(
            err.contains("Device:OPAMP_DUAL"),
            "batch entry must carry the stub error: {out}"
        );
    }

    #[tokio::test]
    async fn pin_locations_resolve_through_lib_name_not_lib_id() {
        // eeschema stores a locally edited library symbol under a derived name
        // and points the instance at it with (lib_name …). Resolving on lib_id
        // alone picks the *base* definition, whose pins sit elsewhere — the
        // wrong answer is returned silently, and every wire placed from it
        // lands off-pin (#143).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250114)\n\t(generator \"eeschema\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(symbol \"R_1_1\"\n\t\t\t\t(pin passive line (at 0 3.81 270) (length 1.27) (name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t\t(symbol \"R_1\"\n\t\t\t(symbol \"R_1_1_1\"\n\t\t\t\t(pin passive line (at 0 6.35 270) (length 1.27) (name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t\t(symbol \"C_1\"\n\t\t\t(symbol \"C_1_1_1\"\n\t\t\t\t(pin passive line (at 0 3.81 270) (length 3.048) (name \"~\") (number \"1\"))\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_name \"R_1\")\n\t\t(lib_id \"Device:R\")\n\t\t(at 88.9 63.5 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-000000000001\")\n\t\t(property \"Reference\" \"R2\" (at 91.44 62.23 0))\n\t)\n\t(symbol\n\t\t(lib_name \"C_1\")\n\t\t(lib_id \"Device:C\")\n\t\t(at 139.7 63.5 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-000000000002\")\n\t\t(property \"Reference\" \"C1\" (at 142.24 62.23 0))\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let res = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "R2" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{}", content_text(&res));
        let out: serde_json::Value = serde_json::from_str(&content_text(&res)).unwrap();
        // R_1's pin sits at local +6.35 => 63.5 - 6.35; Device:R's would be
        // 63.5 - 3.81 = 59.69.
        assert_eq!(out["pins"][0]["y"].as_f64().unwrap(), 57.15);

        // Device:C is not embedded at all — only the derived C_1 is. Matching
        // on lib_id reported "no embedded definition ... nonexistent lib_id",
        // which is both wrong and dangerous advice.
        let batch = handle_batch_get_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["C1"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        let out: serde_json::Value = serde_json::from_str(&content_text(&batch)).unwrap();
        assert!(
            out["components"][0]["error"].is_null(),
            "C1 must resolve through C_1: {out}"
        );
        assert_eq!(
            out["components"][0]["pins"][0]["y"].as_f64().unwrap(),
            59.69
        );
    }

    #[tokio::test]
    async fn replace_component_sets_validated_unit() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("swap.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:OPAMP_DUAL",
                "x": 100.0, "y": 80.0,
                "reference": "U1",
                "unit": 1
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Out-of-range unit on the new symbol is rejected before any write.
        let before = std::fs::read_to_string(&path).unwrap();
        let bad = handle_replace_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "U1",
                "new_lib_id": "Device:OPAMP_DUAL",
                "unit": 99
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(bad.is_error, "unit 99 must be rejected");
        assert!(content_text(&bad).contains("3 unit"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // Valid unit is written to the symbol and its instances entry.
        let ok = handle_replace_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "U1",
                "new_lib_id": "Device:OPAMP_DUAL",
                "unit": 2
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!ok.is_error, "{:?}", ok.content);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("(unit 2)"),
            "unit must be updated to 2:\n{raw}"
        );
        assert!(
            !raw.contains("(unit 1)"),
            "no stale (unit 1) may remain in the instance:\n{raw}"
        );
        let sch = cse::Schematic::load(&path).unwrap();
        assert_eq!(sch.symbols.by_reference("U1").unwrap().unit, 2);
    }

    #[tokio::test]
    async fn add_component_repairs_legacy_file_without_root_uuid() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("legacy.kicad_sch");
        // File shape produced by Konnect before root UUIDs were written.
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 50.0, "y": 50.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("legacy file gains a root uuid");
        let sym = sch.symbols.by_reference("R1").unwrap();
        assert!(sym.has_instance_path("legacy", &format!("/{}", root_uuid)));
    }

    #[tokio::test]
    async fn add_component_with_nonexistent_lib_id_errors_with_suggestion() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("ghost.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Device:CP is the KiCAD ≤9 name; 10 renamed it to C_Polarized (#34).
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:CP",
                "x": 100.0, "y": 80.0,
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error, "nonexistent lib_id must be an error");
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"), "names the bad lib_id: {msg}");
        assert!(
            msg.contains("C_Polarized"),
            "did-you-mean should surface the rename: {msg}"
        );

        // And nothing was written: no ghost instance in the file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn add_component_with_unknown_library_says_so() {
        let (dir, _env) = stub_symbol_dir();
        let path = dir.path().join("nolib.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Transistor_FET_xyzzy:IRF830",
                "x": 100.0, "y": 80.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let msg = format!("{:?}", result.content);
        assert!(
            msg.contains("Library 'Transistor_FET_xyzzy' not found"),
            "distinguishes missing library from missing symbol: {msg}"
        );
    }

    #[tokio::test]
    async fn pin_locations_error_when_definition_not_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noembed.kicad_sch");
        // A symbol instance whose lib_id has NO lib_symbols entry — the file
        // shape a ghost lib_id used to leave behind (#34).
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t)\n\t(symbol\n\t\t(lib_id \"Device:CP\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"C1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_get_schematic_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            result.is_error,
            "missing embedded definition must be an error, not pins: []"
        );
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"));
        assert!(msg.contains("no embedded definition"));
    }

    #[tokio::test]
    async fn add_schematic_component_hides_power_reference() {
        // Pre-seed lib_symbols so ensure_lib_symbol succeeds without KiCad.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("power-via-add.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n  (version 20250610)\n  (generator \"konnect\")\n  (uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n  (paper \"A4\")\n  (lib_symbols\n    (symbol \"power:GND\"\n      (property \"Reference\" \"#PWR\" (at 0 0 0) (hide yes))\n      (property \"Value\" \"GND\" (at 0 0 0))\n    )\n  )\n)\n",
        )
        .unwrap();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "power:GND",
                "x": 50.0,
                "y": 60.0,
                "reference": "#PWR010",
                "value": "GND"
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch
            .symbols
            .iter()
            .find(|s| s.reference() == Some("#PWR010"))
            .expect("power instance");
        let ref_sexp = cse::sexp::writer::write(
            &sym.properties
                .iter()
                .find(|p| p.name == "Reference")
                .unwrap()
                .to_sexp(),
        );
        let hide_at = ref_sexp.find("(hide yes)").expect("property-level hide");
        let effects_at = ref_sexp.find("(effects").expect("effects");
        assert!(
            hide_at < effects_at,
            "power: via add_schematic_component must hide Reference like add_power_symbol: {ref_sexp}"
        );
        let val_sexp = cse::sexp::writer::write(
            &sym.properties
                .iter()
                .find(|p| p.name == "Value")
                .unwrap()
                .to_sexp(),
        );
        assert!(
            !val_sexp.contains("hide"),
            "Value stays visible: {val_sexp}"
        );
    }
}

/// `edit_schematic_component` had two independent defects, both of which
/// reported success: `fields` was declared in the schema and never read
/// (#158), and `new_reference` rewrote only the rendered property, leaving the
/// instances path — which is where KiCad reads the designator for the netlist
/// — on the old value (#157).
#[cfg(test)]
mod edit_component_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    /// One R1, with an instances path, as eeschema writes it.
    const SCH: &str = "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"root\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 50 60 0)\n\t\t(unit 1)\n\t\t(uuid \"sym-1\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 52 58 0)\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 52 62 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"proj\"\n\t\t\t\t(path \"/root\"\n\t\t\t\t\t(reference \"R1\") (unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n\t(sheet_instances\n\t\t(path \"/\" (page \"1\"))\n\t)\n)\n";

    async fn edit(args: serde_json::Value) -> (String, String) {
        let mut f = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        f.write_all(SCH.as_bytes()).unwrap();
        f.flush().unwrap();

        let mut args = args;
        args["schematic"] = json!(f.path().to_str().unwrap());

        let def = tools()
            .into_iter()
            .find(|t| t.name == "edit_schematic_component")
            .unwrap();
        let ctx = Arc::new(ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        ));
        let res = (def.handler)(&args, ctx).await.unwrap();
        let reply = match res.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        (std::fs::read_to_string(f.path()).unwrap(), reply)
    }

    /// #157: the rename must reach the instances path, not just the property.
    #[tokio::test]
    async fn renaming_a_reference_rewrites_the_instances_path() {
        let (out, _) = edit(json!({ "reference": "R1", "new_reference": "R7" })).await;
        assert!(
            out.contains("(property \"Reference\" \"R7\""),
            "property renamed:\n{out}"
        );
        assert!(
            out.contains("(reference \"R7\")"),
            "instances path must carry the new designator, or the netlist \
             ignores the rename:\n{out}"
        );
        assert!(
            !out.contains("(reference \"R1\")"),
            "no instances entry may keep the old designator:\n{out}"
        );
    }

    /// #158: a custom field that does not exist yet must be created.
    #[tokio::test]
    async fn a_new_custom_field_is_written_into_the_symbol() {
        let (out, reply) = edit(json!({
            "reference": "R1",
            "fields": { "MPN": "RC0402FR-0710KL" }
        }))
        .await;
        assert!(
            out.contains("(property \"MPN\" \"RC0402FR-0710KL\""),
            "custom field must land in the file:\n{out}"
        );
        assert!(
            out.contains("(hide yes)"),
            "a custom field is data, not sheet artwork:\n{out}"
        );
        assert!(reply.contains("MPN"), "the reply must report it: {reply}");
        // Anchored on the symbol, not defaulted to the sheet origin (#95).
        assert!(
            !out.contains("(property \"MPN\" \"RC0402FR-0710KL\"\n\t\t\t(at 0 0 0)"),
            "must not land at the sheet origin:\n{out}"
        );
    }

    /// #158: an existing custom field is updated rather than duplicated.
    #[tokio::test]
    async fn an_existing_custom_field_is_updated_not_duplicated() {
        let (out, _) = edit(json!({ "reference": "R1", "fields": { "MPN": "first" } })).await;
        assert_eq!(out.matches("(property \"MPN\"").count(), 1);

        // Value is a first-class parameter, so it must be updated in place.
        let (out2, _) = edit(json!({ "reference": "R1", "value": "22k" })).await;
        assert_eq!(out2.matches("(property \"Value\"").count(), 1, "{out2}");
        assert!(out2.contains("(property \"Value\" \"22k\""), "{out2}");
    }

    /// The defect that made #158 invisible: with `fields` unread, both
    /// `changed` and `errors` came back empty, so the no-op guard never fired
    /// and the call reported success having done nothing.
    #[tokio::test]
    async fn a_fields_only_call_no_longer_reports_an_empty_success() {
        let (_, reply) = edit(json!({
            "reference": "R1",
            "fields": { "MPN": "RC0402FR-0710KL" }
        }))
        .await;
        assert!(
            !reply.contains("\"changes\":[]"),
            "a fields-only call must not report an empty change set: {reply}"
        );
    }

    /// Reserved names belong to their own parameters — routing Reference
    /// through `fields` would skip the instances rewrite and silently
    /// reintroduce #157.
    #[tokio::test]
    async fn reserved_names_are_refused_inside_fields() {
        let (out, reply) = edit(json!({
            "reference": "R1",
            "fields": { "Reference": "R9" }
        }))
        .await;
        assert!(
            out.contains("(property \"Reference\" \"R1\""),
            "the designator must be untouched:\n{out}"
        );
        assert!(
            reply.contains("Reference"),
            "the refusal is reported: {reply}"
        );
    }
}

#[cfg(test)]
mod move_connected_tests {
    use super::*;

    const TOL: f64 = 0.01;

    #[test]
    fn a_wire_ending_at_the_point_is_not_passing_through() {
        // Two stubs meeting at the pin. Both follow the pin, so the junction
        // between them should follow it too.
        let segs = [((10.0, 10.0), (20.0, 10.0)), ((20.0, 10.0), (20.0, 30.0))];
        assert!(!wire_passes_through(&segs, 20.0, 10.0, TOL));
    }

    #[test]
    fn a_wire_crossing_the_point_is_passing_through() {
        // A bus running past the pin, with a stub tapping it. The dot holds
        // that T together and must not be dragged off the bus.
        let segs = [((0.0, 10.0), (40.0, 10.0)), ((20.0, 10.0), (20.0, 30.0))];
        assert!(wire_passes_through(&segs, 20.0, 10.0, TOL));
    }

    #[test]
    fn a_point_off_every_wire_is_not_passing_through() {
        let segs = [((0.0, 10.0), (40.0, 10.0))];
        assert!(!wire_passes_through(&segs, 20.0, 25.0, TOL));
    }

    #[test]
    fn vertical_wires_are_handled_too() {
        let segs = [((5.0, 0.0), (5.0, 50.0))];
        assert!(wire_passes_through(&segs, 5.0, 25.0, TOL), "midpoint");
        assert!(!wire_passes_through(&segs, 5.0, 0.0, TOL), "endpoint");
        assert!(!wire_passes_through(&segs, 5.0, 50.0, TOL), "endpoint");
    }

    #[test]
    fn no_wires_at_all_means_nothing_passes_through() {
        // The J6 case: a connector held entirely by labels, no wires anywhere
        // near it. Every junction on such a pin is safe to carry along.
        assert!(!wire_passes_through(&[], 175.26, 99.06, TOL));
    }
}
