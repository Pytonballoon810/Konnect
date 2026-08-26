//! `project` toolset — create, open, save, and snapshot KiCAD projects.
//!
//! Tools: create_project, open_project, save_project, get_project_info, snapshot_project
//!
//! KiCAD interface:
//!   - create_project   → file system (template)
//!   - open_project     → IPC ping (check if project is open)
//!   - save_project     → IPC board.save()
//!   - get_project_info → file system read
//!   - snapshot_project → kicad-cli export PDF

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{get_path, opt_str, require_str, ToolContext, ToolDef};
use konnect_sexp::{commit_file_transaction, FileTransition, SexpError};
use serde_json::json;
use std::path::{Path, PathBuf};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_project",
            "Create a new KiCAD project at the given path. Creates the directory, \
             a blank .kicad_pro file, an empty .kicad_sch schematic, and a blank \
             .kicad_pcb board file. Refuses to replace any existing project file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path where the project will be created"
                    },
                    "name": {
                        "type": "string",
                        "description": "Project name (used as filename stem)"
                    }
                },
                "required": ["path", "name"]
            }),
            |args, ctx| async move { handle_create_project(args, ctx).await }
        ),
        tool!(
            "open_project",
            "Check whether a KiCAD project is currently open in the running KiCAD UI. \
             Returns the active project path and whether KiCAD IPC is available.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Optional: path to .kicad_pro file to check"
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_open_project(args, ctx).await }
        ),
        tool!(
            "save_project",
            "Save the currently open PCB board file via KiCAD IPC. \
             Requires KiCAD to be running with IPC enabled.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_save_project(args, ctx).await }
        ),
        tool!(
            "get_project_info",
            "Read project metadata from a .kicad_pro file. Returns the project name, \
             schematic and PCB paths, and last modified times.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the .kicad_pro project file"
                    }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_get_project_info(args, ctx).await }
        ),
        tool!(
            "snapshot_project",
            "Export the schematic and PCB to PDF as a timestamped snapshot/checkpoint. \
             Useful for saving progress before major edits.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": {
                        "type": "string",
                        "description": "Path to .kicad_sch file"
                    },
                    "pcb": {
                        "type": "string",
                        "description": "Optional: path to .kicad_pcb file"
                    },
                    "output_dir": {
                        "type": "string",
                        "description": "Directory to write snapshot PDFs"
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional label to include in the filename"
                    }
                },
                "required": ["schematic", "output_dir"]
            }),
            |args, ctx| async move { handle_snapshot_project(args, ctx).await }
        ),
        tool!(
            "open_schematic_viewer",
            "Launch the live schematic viewer. The viewer shows the schematic as SVG and \
             auto-refreshes when the file changes. Use this after placing components so the \
             user can see the schematic in real-time as you edit it. For hierarchical designs, \
             pass the root schematic — the viewer discovers every sheet reachable from it and \
             shows a sheet selector.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": {
                        "type": "string",
                        "description": "Path to the root .kicad_sch file to view"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_open_viewer(args, ctx).await }
        ),
        tool!(
            "set_live_view",
            "Keep what is on screen in step with what you write, for the rest of the session. \
             Off by default. With it on, writing a sheet tells the schematic viewer to show \
             that sheet, so the user watches the design change as you work. \
             `reload_open_boards` additionally pushes board writes into an open pcbnew — \
             leave it off unless the user has agreed to it, because KiCAD reloads a document \
             without asking and DISCARDS whatever they had unsaved in that board (measured: \
             no prompt, no warning, the unsaved edit simply gone). It also costs a round-trip \
             to KiCAD on every board write. Schematics cannot be updated this way at all: \
             KiCAD 10 exposes no schematic API, which is what the viewer is for.",
            json!({
                "type": "object",
                "properties": {
                    "enabled": {
                        "type": "boolean",
                        "description": "Turn live view on or off"
                    },
                    "reload_open_boards": {
                        "type": "boolean",
                        "description": "Also reload boards in an open pcbnew after writing them. Destroys unsaved work in that board — ask the user first. Default false.",
                        "default": false
                    }
                },
                "required": ["enabled"]
            }),
            |args, ctx| async move { handle_set_live_view(args, ctx).await }
        ),
        tool!(
            "reload_kicad_view",
            "Make an open pcbnew show what is now on disk for one board — the one-shot form of \
             `set_live_view`'s board half. Use it after editing a board file that KiCAD already \
             has open, otherwise the user sees a stale board and KiCAD's next save overwrites \
             your edit. Reports `reloaded: false` when KiCAD does not have that board open, \
             which is not an error. DISCARDS unsaved changes in that board without prompting; \
             only call it when the user has agreed to that.",
            json!({
                "type": "object",
                "properties": {
                    "board": {
                        "type": "string",
                        "description": "Path to the .kicad_pcb file to reload"
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_reload_kicad_view(args, ctx).await }
        ),
        tool!(
            "focus_schematic_view",
            "Point the running schematic viewer at one sheet, and optionally ring some symbols \
             on it. Use it to show the user what you are working on before you change it — the \
             viewer follows the file itself, but it cannot know which of twenty sheets matters \
             right now, or which two resistors you just moved. Harmless when no viewer is \
             running: it leaves a message the next viewer picks up.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": {
                        "type": "string",
                        "description": "Path to the .kicad_sch sheet to bring to the front"
                    },
                    "highlight": {
                        "type": "array",
                        "description": "References to ring on that sheet, e.g. [\"R4\", \"C12\"]",
                        "items": { "type": "string" }
                    },
                    "reason": {
                        "type": "string",
                        "description": "Short note shown to the user, e.g. 'adding decoupling'"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_focus_schematic_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

fn existing_project_paths(paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut existing = Vec::new();
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Ok(_) => existing.push(path.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(existing)
}

fn project_conflict(paths: Vec<PathBuf>) -> CallToolResult {
    let paths: Vec<String> = paths
        .into_iter()
        .map(|path| path.display().to_string())
        .collect();
    CallToolResult::error_kind(
        ToolErrorKind::Conflict {
            paths: paths.clone(),
        },
        format!(
            "Project creation would replace existing path(s): {}",
            paths.join(", ")
        ),
    )
}

fn create_project_files(
    project_dir: &Path,
    name: &str,
    pro_path: &Path,
    sch_path: &Path,
    pcb_path: &Path,
) -> Result<(), SexpError> {
    commit_file_transaction(
        project_dir,
        vec![
            FileTransition::create(pro_path, blank_kicad_pro(name)),
            FileTransition::create(sch_path, blank_kicad_sch()),
            FileTransition::create(pcb_path, blank_kicad_pcb()),
        ],
    )?;
    Ok(())
}

async fn handle_create_project(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;
    let name = match require_str(args, "name") {
        Ok(n) => n.to_string(),
        Err(e) => return Ok(e),
    };

    let pro_path = path.join(format!("{}.kicad_pro", name));
    let sch_path = path.join(format!("{}.kicad_sch", name));
    let pcb_path = path.join(format!("{}.kicad_pcb", name));
    let project_paths = vec![pro_path.clone(), sch_path.clone(), pcb_path.clone()];

    let existing = tokio::task::spawn_blocking({
        let project_paths = project_paths.clone();
        move || existing_project_paths(&project_paths)
    })
    .await??;
    if !existing.is_empty() {
        return Ok(project_conflict(existing));
    }

    tokio::fs::create_dir_all(&path).await?;

    let transaction = tokio::task::spawn_blocking({
        let project_dir = path.clone();
        let name = name.clone();
        let pro_path = pro_path.clone();
        let sch_path = sch_path.clone();
        let pcb_path = pcb_path.clone();
        move || create_project_files(&project_dir, &name, &pro_path, &sch_path, &pcb_path)
    })
    .await?;
    match transaction {
        Ok(()) => {}
        Err(SexpError::TransactionConflict { path, .. }) => {
            return Ok(project_conflict(vec![path]));
        }
        Err(error) => return Err(error.into()),
    }

    Ok(CallToolResult::json(&json!({
        "created": true,
        "project_file": pro_path.display().to_string(),
        "schematic": sch_path.display().to_string(),
        "pcb": pcb_path.display().to_string()
    })))
}

async fn handle_open_project(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let ipc = konnect_ipc::KiCadIpcClient::new(&ctx.config.ipc_address);
    let connected = ipc.ping().unwrap_or(false);

    Ok(CallToolResult::json(&json!({
        "kicad_ui_running": connected,
        "ipc_address": ctx.config.ipc_address,
        "message": if connected {
            "KiCAD is running and IPC is available."
        } else {
            "KiCAD IPC is not reachable. Start KiCAD and enable the IPC API, or work in file-only mode."
        }
    })))
}

async fn handle_save_project(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let ipc = konnect_ipc::KiCadIpcClient::new(&ctx.config.ipc_address);
    ipc.save_board()?;
    Ok(CallToolResult::text("Board saved successfully."))
}

async fn handle_get_project_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;

    // Demonstration of the structured-error pattern: returning a FileNotFound
    // kind lets clients branch (e.g. show a file picker) without string-parsing
    // the message.
    if !path.exists() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: path.display().to_string(),
            },
            format!("Project file not found: {}", path.display()),
        ));
    }

    let content = tokio::fs::read_to_string(&path).await?;
    let pro: serde_json::Value = serde_json::from_str(&content)?;

    let dir = path.parent().unwrap_or(&path);
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let sch = dir.join(format!("{}.kicad_sch", stem));
    let pcb = dir.join(format!("{}.kicad_pcb", stem));

    let meta = tokio::fs::metadata(&path).await.ok();
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    Ok(CallToolResult::json(&json!({
        "name": stem,
        "path": path.display().to_string(),
        "schematic": sch.display().to_string(),
        "schematic_exists": sch.exists(),
        "pcb": pcb.display().to_string(),
        "pcb_exists": pcb.exists(),
        "last_modified_unix": modified,
        "kicad_version": pro.get("meta").and_then(|m| m.get("filename")).and_then(|v| v.as_str())
    })))
}

async fn handle_snapshot_project(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic = get_path(args, "schematic")?;
    let output_dir = get_path(args, "output_dir")?;
    let label = opt_str(args, "label").unwrap_or("snapshot");

    tokio::fs::create_dir_all(&output_dir).await?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let stem = schematic
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let pdf_name = format!("{}_{}_{}.pdf", stem, label, ts);
    let pdf_path = output_dir.join(&pdf_name);

    crate::tools::cli::export_schematic_pdf(&ctx.config.kicad_cli, &schematic, &pdf_path).await?;

    let mut result = json!({
        "snapshot": pdf_path.display().to_string(),
        "label": label,
        "timestamp": ts
    });

    // Optionally snapshot PCB too
    if let Some(pcb_str) = opt_str(args, "pcb") {
        let pcb = PathBuf::from(pcb_str);
        let pcb_pdf_name = format!("{}_pcb_{}_{}.pdf", stem, label, ts);
        let pcb_pdf_path = output_dir.join(&pcb_pdf_name);
        let layers = &["F.Cu", "B.Cu", "F.Silkscreen", "B.Silkscreen", "Edge.Cuts"];
        let _ =
            crate::tools::cli::export_pdf(&ctx.config.kicad_cli, &pcb, &pcb_pdf_path, layers).await;
        result["pcb_snapshot"] = json!(pcb_pdf_path.display().to_string());
    }

    Ok(CallToolResult::json(&result))
}

async fn handle_open_viewer(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;

    if !sch_path.exists() {
        return Ok(CallToolResult::error(format!(
            "File not found: {}",
            sch_path.display()
        )));
    }

    // Find the viewer binary — it should be next to the konnect binary
    let viewer_binary = find_viewer_binary();

    match viewer_binary {
        Some(viewer_path) => {
            tracing::info!(
                "[BETA] Launching schematic viewer: {} {}",
                viewer_path.display(),
                sch_path.display()
            );

            // Spawn as detached process, forwarding the configured kicad-cli
            // path so the viewer renders with the same binary we use.
            let mut cmd = std::process::Command::new(&viewer_path);
            if !ctx.config.kicad_cli.is_empty() {
                cmd.arg("--kicad-cli").arg(&ctx.config.kicad_cli);
            }
            let child = cmd.arg(&sch_path).spawn();

            match child {
                Ok(_) => Ok(CallToolResult::text(
                    serde_json::to_string(&json!({
                        "launched": true,
                        "viewer": viewer_path.to_str().unwrap_or(""),
                        "schematic": sch_path.to_str().unwrap_or(""),
                        "note": "Schematic viewer opened. It will auto-refresh as you make changes to the schematic file."
                    }))
                    .unwrap(),
                )),
                Err(e) => Ok(CallToolResult::error(format!("Failed to launch viewer: {}", e))),
            }
        }
        None => Ok(CallToolResult::error(
            "Schematic viewer binary (schematic-viewer.exe) not found. \
             It should be in the same directory as konnect.exe.",
        )),
    }
}

async fn handle_set_live_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let enabled = match args["enabled"].as_bool() {
        Some(enabled) => enabled,
        None => return Ok(CallToolResult::error("'enabled' must be true or false")),
    };
    let reload_open_boards = args["reload_open_boards"].as_bool().unwrap_or(false);

    super::live_view::configure(enabled, reload_open_boards, ctx.config.ipc_address.clone());

    // Say plainly what was turned on. "Live view enabled" reads as one
    // feature; it is two, and only one of them can lose the user's work.
    let note = match (enabled, reload_open_boards) {
        (false, _) => "Live view off. Writes update nothing on screen.".to_string(),
        (true, false) => "Live view on for the schematic viewer only: writing a sheet brings \
                          it to the front. Boards in an open pcbnew are NOT reloaded, so \
                          pcbnew keeps showing its own copy until someone reverts it."
            .to_string(),
        (true, true) => "Live view on, including boards. Writing a .kicad_pcb reloads it in \
                         an open pcbnew, DISCARDING anything unsaved there without prompting, \
                         and costs a round-trip to KiCAD per write."
            .to_string(),
    };

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "enabled": enabled,
            "reload_open_boards": reload_open_boards,
            "schematic_strategy": "viewer focus file (KiCAD 10 exposes no schematic API)",
            "ipc_address": super::live_view::effective_ipc_address(&ctx.config.ipc_address),
            "note": note,
        }))
        .unwrap(),
    ))
}

async fn handle_reload_kicad_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    if !board.exists() {
        return Ok(CallToolResult::error(format!(
            "File not found: {}",
            board.display()
        )));
    }

    // Same resolution as set_live_view: this tool exists to reach an open
    // KiCAD, so an unset address means "KiCAD's usual place", not "give up".
    let address = super::live_view::effective_ipc_address(&ctx.config.ipc_address);
    let board_for_task = board.clone();
    let reloaded = tokio::task::spawn_blocking(move || {
        konnect_ipc::client::KiCadIpcClient::new(address).reload_from_disk(&board_for_task)
    })
    .await?;

    match reloaded {
        Ok(true) => Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "reloaded": true,
                "board": board.to_string_lossy(),
                "note": "pcbnew now shows the file on disk. Anything unsaved in that board is gone."
            }))
            .unwrap(),
        )),
        Ok(false) => Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "reloaded": false,
                "board": board.to_string_lossy(),
                "reason": "KiCAD does not have this board open — there is no view to update."
            }))
            .unwrap(),
        )),
        Err(error) => Ok(CallToolResult::error(format!(
            "Could not reload {} in KiCAD: {error:#}",
            board.display()
        ))),
    }
}

async fn handle_focus_schematic_view(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sheet = get_path(args, "schematic")?;
    if !sheet.exists() {
        return Ok(CallToolResult::error(format!(
            "File not found: {}",
            sheet.display()
        )));
    }

    let highlight: Vec<String> = args["highlight"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let reason = args["reason"].as_str().unwrap_or("focus");

    match super::live_view::write_focus(&sheet, &highlight, reason) {
        Ok(seq) => Ok(CallToolResult::text(
            serde_json::to_string(&json!({
                "focused": sheet.to_string_lossy(),
                "highlight": highlight,
                "seq": seq,
                "note": "A running viewer switches to this sheet. With none running, the \
                         message waits for the next one."
            }))
            .unwrap(),
        )),
        Err(error) => Ok(CallToolResult::error(format!(
            "Could not post viewer focus: {error}"
        ))),
    }
}

fn find_viewer_binary() -> Option<std::path::PathBuf> {
    // Check next to the current executable
    if let Ok(exe_path) = std::env::current_exe() {
        let dir = exe_path.parent()?;
        let viewer = dir.join(if cfg!(target_os = "windows") {
            "schematic-viewer.exe"
        } else {
            "schematic-viewer"
        });
        if viewer.exists() {
            return Some(viewer);
        }
    }

    // Check common locations
    let candidates = ["schematic-viewer.exe", "schematic-viewer"];
    for c in &candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

// ─── Blank file templates ──────────────────────────────────────────────────────

fn blank_kicad_pro(name: &str) -> String {
    format!(
        r#"{{
  "meta": {{
    "filename": "{name}.kicad_pro",
    "version": 1
  }},
  "board": {{
    "design_settings": {{}}
  }},
  "schematic": {{
    "legacy_lib_dir": "",
    "legacy_lib_list": []
  }}
}}
"#
    )
}

fn blank_kicad_sch() -> String {
    crate::tools::blank_schematic_template()
}

fn blank_kicad_pcb() -> &'static str {
    "(kicad_pcb\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(general\n\t\t(thickness 1.6)\n\t)\n\t(paper \"A4\")\n\t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(31 \"B.Cu\" signal)\n\t\t(32 \"B.Adhes\" user \"B.Adhesive\")\n\t\t(33 \"F.Adhes\" user \"F.Adhesive\")\n\t\t(34 \"B.Paste\" user)\n\t\t(35 \"F.Paste\" user)\n\t\t(36 \"B.SilkS\" user \"B.Silkscreen\")\n\t\t(37 \"F.SilkS\" user \"F.Silkscreen\")\n\t\t(38 \"B.Mask\" user)\n\t\t(39 \"F.Mask\" user)\n\t\t(40 \"Dwgs.User\" user \"User.Drawings\")\n\t\t(41 \"Cmts.User\" user \"User.Comments\")\n\t\t(44 \"Edge.Cuts\" user)\n\t\t(45 \"Margin\" user)\n\t\t(46 \"B.CrtYd\" user \"B.Courtyard\")\n\t\t(47 \"F.CrtYd\" user \"F.Courtyard\")\n\t\t(48 \"B.Fab\" user)\n\t\t(49 \"F.Fab\" user)\n\t)\n\t(setup\n\t\t(pad_to_mask_clearance 0.05)\n\t)\n\t(net 0 \"\")\n)\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
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

    // ─── Blank file templates ─────────────────────────────────────────────

    #[test]
    fn blank_kicad_pro_is_valid_json_with_name() {
        let content = blank_kicad_pro("my_board");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["meta"]["filename"], "my_board.kicad_pro");
    }

    #[test]
    fn blank_kicad_sch_has_expected_header() {
        let content = blank_kicad_sch();
        assert!(content.starts_with("(kicad_sch"));
        assert!(content.contains("(lib_symbols"));
    }

    #[test]
    fn blank_kicad_sch_has_root_uuid() {
        // Without a root (uuid ...) KiCAD's netlister silently drops every
        // wire-only net (symbol instance paths can't resolve).
        let content = blank_kicad_sch();
        assert!(content.contains("(uuid \""));
        let sch = {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("t.kicad_sch");
            std::fs::write(&path, &content).unwrap();
            konnect_schematic_editor::Schematic::load(&path).unwrap()
        };
        let uuid = sch.uuid.expect("blank schematic must carry a root uuid");
        assert_eq!(uuid.len(), 36, "expected a v4 uuid, got '{uuid}'");
    }

    #[test]
    fn blank_kicad_pcb_declares_core_layers() {
        let content = blank_kicad_pcb();
        assert!(content.contains("\"F.Cu\""));
        assert!(content.contains("\"B.Cu\""));
        assert!(content.contains("\"Edge.Cuts\""));
    }

    // ─── handle_create_project ─────────────────────────────────────────────

    #[tokio::test]
    async fn create_project_writes_all_three_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("handler should succeed");
        assert!(!result.is_error);

        assert!(dir.path().join("widget.kicad_pro").exists());
        assert!(dir.path().join("widget.kicad_sch").exists());
        assert!(dir.path().join("widget.kicad_pcb").exists());

        let pro_content = tokio::fs::read_to_string(dir.path().join("widget.kicad_pro"))
            .await
            .unwrap();
        let pro: serde_json::Value = serde_json::from_str(&pro_content).unwrap();
        assert_eq!(pro["meta"]["filename"], "widget.kicad_pro");
    }

    #[tokio::test]
    async fn create_project_rejects_a_partially_populated_directory_without_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let existing_path = dir.path().join("widget.kicad_sch");
        tokio::fs::write(&existing_path, b"existing schematic")
            .await
            .unwrap();
        let ctx = test_ctx();
        let args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("handler should return a structured conflict");

        assert!(result.is_error);
        assert_eq!(extract_error_kind(&result).as_deref(), Some("conflict"));
        let body = response_json(&result);
        assert_eq!(
            body["error"]["paths"],
            json!([existing_path.display().to_string()])
        );
        assert_eq!(
            tokio::fs::read(&existing_path).await.unwrap(),
            b"existing schematic"
        );
        assert!(!dir.path().join("widget.kicad_pro").exists());
        assert!(!dir.path().join("widget.kicad_pcb").exists());
    }

    #[tokio::test]
    async fn create_project_rejects_a_completed_project_without_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });
        handle_create_project(&args, &ctx)
            .await
            .expect("initial creation should succeed");
        let paths = [
            dir.path().join("widget.kicad_pro"),
            dir.path().join("widget.kicad_sch"),
            dir.path().join("widget.kicad_pcb"),
        ];
        let before = [
            tokio::fs::read(&paths[0]).await.unwrap(),
            tokio::fs::read(&paths[1]).await.unwrap(),
            tokio::fs::read(&paths[2]).await.unwrap(),
        ];

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("repeat creation should return a conflict");

        assert!(result.is_error);
        assert_eq!(extract_error_kind(&result).as_deref(), Some("conflict"));
        let body = response_json(&result);
        assert_eq!(body["error"]["paths"].as_array().unwrap().len(), 3);
        for (path, original) in paths.iter().zip(before) {
            assert_eq!(tokio::fs::read(path).await.unwrap(), original);
        }
    }

    #[tokio::test]
    async fn create_project_missing_name_returns_structured_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let args = json!({ "path": dir.path().to_str().unwrap() });

        let result = handle_create_project(&args, &ctx)
            .await
            .expect("handler should return Ok even on validation failure");
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
    }

    // ─── handle_get_project_info ───────────────────────────────────────────

    #[tokio::test]
    async fn get_project_info_reports_existing_sibling_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let create_args = json!({
            "path": dir.path().to_str().unwrap(),
            "name": "widget"
        });
        handle_create_project(&create_args, &ctx)
            .await
            .expect("setup: create_project should succeed");

        let pro_path = dir.path().join("widget.kicad_pro");
        let info_args = json!({ "path": pro_path.to_str().unwrap() });
        let result = handle_get_project_info(&info_args, &ctx)
            .await
            .expect("handler should succeed");
        assert!(!result.is_error);

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["name"], "widget");
        assert_eq!(parsed["schematic_exists"], true);
        assert_eq!(parsed["pcb_exists"], true);
    }

    #[tokio::test]
    async fn get_project_info_missing_file_returns_file_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_ctx();
        let missing = dir.path().join("does_not_exist.kicad_pro");
        let args = json!({ "path": missing.to_str().unwrap() });

        let result = handle_get_project_info(&args, &ctx)
            .await
            .expect("handler should return Ok with a structured error body");
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("file_not_found")
        );
    }

    fn response_json(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }
}
