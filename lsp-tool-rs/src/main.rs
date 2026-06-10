//! lsp-tool (v0) — a stateless, hand-rolled LSP CLI that drives ty.
//!
//! Subcommands emit JSON on stdout (the server's own logs go to stderr):
//!   lsp-tool diagnostics <file>
//!   lsp-tool rename <file> <target> <new-name> [--apply]
//!   lsp-tool references <file> <target>
//!
//! `<target>` is a symbol name (`greet`) — the tool resolves it to protocol
//! coordinates so the caller never counts columns — or an explicit
//! `line:character` position (zero-indexed; `character` in UTF-16 units) as
//! the escape hatch for ambiguous symbols.
//!
//! Errors are JSON too: `{"error": {"message", "data"?}}` on stdout, exit 1 —
//! an agent should never have to parse a panic off stderr.

mod apply;
mod client;
mod resolve;
mod transport;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use client::Client;
use resolve::{parse_target, resolve_symbol, Target};

/// A failure with a machine-readable payload (e.g. the candidate list behind
/// an "ambiguous symbol" error), surfaced under `error.data` in the JSON.
#[derive(Debug)]
pub struct ToolError {
    pub message: String,
    pub data: Option<Value>,
}

impl ToolError {
    pub fn new(message: String) -> Self {
        ToolError { message, data: None }
    }
    pub fn with_data(message: String, data: Value) -> Self {
        ToolError { message, data: Some(data) }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

#[derive(Parser)]
#[command(name = "lsp-tool", version, about = "Stateless LSP rename/references/diagnostics CLI (v0, ty)")]
struct Cli {
    /// Workspace root (rootUri). File paths are resolved against it.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    /// Language-server command (whitespace-split). Default is the uv-managed ty
    /// binary (`uv sync` puts it at `.venv/bin/ty`) — exec'd directly, no `uv
    /// run` wrapper. Override per language.
    #[arg(long, default_value = ".venv/bin/ty server")]
    server_cmd: String,

    /// Seconds to wait for any single server response before failing.
    #[arg(long, default_value_t = 30)]
    timeout: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report diagnostics for a file.
    Diagnostics {
        file: String,
    },
    /// Rename a symbol; prints the WorkspaceEdit (or applies it with --apply).
    Rename {
        file: String,
        /// Symbol name, or `line:character` (zero-indexed, UTF-16 column).
        target: String,
        new_name: String,
        /// Apply the edit to disk instead of just printing it.
        #[arg(long)]
        apply: bool,
    },
    /// List every reference to a symbol (declaration included).
    References {
        file: String,
        /// Symbol name, or `line:character` (zero-indexed, UTF-16 column).
        target: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(output) => println!("{}", serde_json::to_string_pretty(&output).unwrap()),
        Err(e) => {
            let mut error = json!({"message": format!("{e:#}")});
            if let Some(data) = e.downcast_ref::<ToolError>().and_then(|t| t.data.clone()) {
                error["data"] = data;
            }
            println!("{}", serde_json::to_string_pretty(&json!({"error": error})).unwrap());
            std::process::exit(1);
        }
    }
}

fn run(cli: &Cli) -> Result<Value> {
    let workspace = cli.workspace.canonicalize().unwrap_or(cli.workspace.clone());
    let mut client = Client::start(&cli.server_cmd, &workspace, Duration::from_secs(cli.timeout))?;

    let output: Value = match &cli.command {
        Command::Diagnostics { file } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, language_id(&abs))?;
            let diagnostics = client.diagnostics(&uri)?;
            json!({"file": file, "diagnostics": diagnostics})
        }
        Command::Rename { file, target, new_name, apply } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, language_id(&abs))?;
            let (line, character, resolved_from) = locate(&mut client, &uri, &abs, file, target)?;
            let edit = client.rename(&uri, line, character, new_name)?;
            let target_json = json!({"line": line, "character": character, "resolved_from": resolved_from});
            if *apply {
                let changed = apply::apply_workspace_edit(&edit)?;
                json!({"applied": true, "target": target_json, "files_changed": changed, "edit": edit})
            } else {
                json!({"applied": false, "target": target_json, "edit": edit})
            }
        }
        Command::References { file, target } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, language_id(&abs))?;
            let (line, character, resolved_from) = locate(&mut client, &uri, &abs, file, target)?;
            let locations = client.references(&uri, line, character)?;
            let references = render_locations(&workspace, &locations)?;
            json!({
                "file": file,
                "target": {"line": line, "character": character, "resolved_from": resolved_from},
                "count": references.len(),
                "references": references,
            })
        }
    };

    client.shutdown()?;
    Ok(output)
}

/// Turn a CLI target into a position: pass `line:char` through, resolve a
/// symbol name via resolve.rs.
fn locate(
    client: &mut Client,
    uri: &str,
    abs: &Path,
    file: &str,
    target: &str,
) -> Result<(usize, usize, &'static str)> {
    match parse_target(target) {
        Target::Position { line, character } => Ok((line, character, "position")),
        Target::Symbol(name) => {
            let text = std::fs::read_to_string(abs)?;
            let (line, character) = resolve_symbol(client, uri, &text, file, &name)?;
            Ok((line, character, "symbol"))
        }
    }
}

/// LSP Locations → `{file, line, character, text}` rows, paths relative to the
/// workspace and `text` the (trimmed) source line, so an agent can read the
/// result without another round of file opens.
fn render_locations(workspace: &Path, locations: &[Value]) -> Result<Vec<Value>> {
    let mut texts: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut rows = Vec::new();
    for loc in locations {
        let uri = loc["uri"].as_str().unwrap_or_default();
        let path = apply::uri_to_path(uri);
        let line = loc["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
        let character = loc["range"]["start"]["character"].as_u64().unwrap_or(0) as usize;
        let lines = match texts.entry(path.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(
                std::fs::read_to_string(&path)
                    .map(|t| t.lines().map(|l| l.trim_end().to_string()).collect())
                    .unwrap_or_default(),
            ),
        };
        let text = lines.get(line).cloned().unwrap_or_default();
        let rel = path.strip_prefix(workspace).unwrap_or(&path).display().to_string();
        rows.push(json!({"file": rel, "line": line, "character": character, "text": text}));
    }
    rows.sort_by(|a, b| {
        (a["file"].as_str(), a["line"].as_u64(), a["character"].as_u64())
            .cmp(&(b["file"].as_str(), b["line"].as_u64(), b["character"].as_u64()))
    });
    Ok(rows)
}

/// LSP languageId from the file extension. `--server-cmd` picks the server;
/// this just stops `didOpen` from lying about non-Python files.
fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "py" | "pyi" => "python",
        "go" => "go",
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        _ => "plaintext",
    }
}
