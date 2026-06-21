//! lsp4a (v0) — a stateless, hand-rolled LSP CLI for agents, driving ty.
//!
//! Subcommands emit JSON on stdout (the server's own logs go to stderr):
//!   lsp4a diagnostics <file>
//!   lsp4a rename <file> <target> <new-name> [--apply] [--raw]
//!   lsp4a references <file> <target>
//!
//! `rename` returns a structured summary — status, scope (files/edits), and a
//! before/after row per changed line — never a raw WorkspaceEdit (that's behind
//! `--raw`). The agent reads the result without parsing protocol coordinates.
//!
//! `<target>` is a symbol name (`greet`) — the tool resolves it to protocol
//! coordinates so the caller never counts columns — or an explicit
//! `line:character` position (zero-indexed; `character` in UTF-16 units) as
//! the escape hatch for ambiguous symbols.
//!
//! Errors are JSON too: `{"error": {"message", "data"?}}` on stdout — exit 1 for
//! runtime failures, exit 2 for usage errors (bad arguments; these add `usage`).
//! Even clap's parse errors are funneled here, so an agent never parses prose off
//! stderr. The server's own logs stay on stderr, suppressed unless `--debug`.

mod apply;
mod client;
mod resolve;
mod transport;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::error::{ContextKind, ContextValue, ErrorKind};
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
#[command(name = "lsp4a", version, about = "Stateless LSP rename/references/diagnostics CLI for agents (v0, ty)")]
struct Cli {
    /// Workspace root (rootUri). File paths are resolved against it.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    /// Language-server command (whitespace-split). Defaults to `ty` on PATH —
    /// ty is a standalone binary (no Python); install it with
    /// `curl -LsSf https://astral.sh/ty/install.sh | sh`. Override per language.
    #[arg(long, default_value = "ty server")]
    server_cmd: String,

    /// Seconds to wait for any single server response before failing.
    #[arg(long, default_value_t = 30)]
    timeout: u64,

    /// Surface the language server's own logs (its stderr). Off by default so
    /// the JSON result isn't drowned in server chatter. Accepted in any position.
    #[arg(long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report diagnostics for a file.
    Diagnostics {
        file: String,
        /// Also include the raw LSP diagnostics under `raw`.
        #[arg(long)]
        raw: bool,
    },
    /// Rename a symbol; prints a structured before/after summary of the change
    /// (or applies it to disk with --apply).
    Rename {
        file: String,
        /// Symbol name, or `line:character` (zero-indexed, UTF-16 column).
        target: String,
        new_name: String,
        /// Apply the edit to disk instead of only previewing it.
        #[arg(long)]
        apply: bool,
        /// Also include the raw LSP WorkspaceEdit under `edit` (for callers that
        /// want to apply it themselves). Off by default — the summary is the contract.
        #[arg(long)]
        raw: bool,
    },
    /// List every reference to a symbol (declaration included).
    References {
        file: String,
        /// Symbol name, or `line:character` (zero-indexed, UTF-16 column).
        target: String,
        /// Also include the raw LSP Location list under `locations`.
        #[arg(long)]
        raw: bool,
    },
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => clap_exit(err),
    };
    match run(&cli) {
        Ok(output) => println!("{}", serde_json::to_string_pretty(&output).unwrap()),
        Err(e) => {
            let mut error = json!({"message": format!("{e:#}")});
            if let Some(data) = e.downcast_ref::<ToolError>().and_then(|t| t.data.clone()) {
                error["data"] = data;
            }
            emit_error(error, 1);
        }
    }
}

/// Print `{"error": ...}` on stdout and exit — the single error path every
/// failure funnels through, so an agent only ever has to parse stdout JSON.
fn emit_error(error: Value, code: i32) -> ! {
    println!("{}", serde_json::to_string_pretty(&json!({"error": error})).unwrap());
    std::process::exit(code);
}

/// Resolve a clap parse failure. `--help`/`--version` aren't errors — let clap
/// print them and exit 0. Everything else is a usage error: surface it as the
/// same crisp JSON envelope as every other failure (never clap's multi-line
/// preamble on stderr), with the usage line, and exit 2 — the conventional
/// "command-line usage" code, distinct from a runtime failure's exit 1.
fn clap_exit(err: clap::Error) -> ! {
    if matches!(err.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        print!("{err}");
        std::process::exit(0);
    }
    let mut error = json!({"message": clap_message(&err)});
    if let Some(usage) = clap_usage(&err) {
        error["usage"] = json!(usage);
    }
    emit_error(error, 2);
}

/// A crisp one-line message for a clap usage error. The missing-arguments and
/// missing-subcommand cases get purpose-built messages; other kinds reuse clap's
/// own `error: <summary>` line. The fallback is generic — never clap's help text
/// (a missing subcommand renders help-style, with no `error:` line at all).
fn clap_message(err: &clap::Error) -> String {
    match err.kind() {
        ErrorKind::MissingRequiredArgument => {
            if let Some(ContextValue::Strings(args)) = err.get(ContextKind::InvalidArg) {
                return format!("missing required argument(s): {}", args.join(", "));
            }
        }
        ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            return "a subcommand is required: rename, references, or diagnostics".to_string();
        }
        _ => {}
    }
    err.to_string()
        .lines()
        .find_map(|l| l.strip_prefix("error: "))
        .map(str::to_string)
        .unwrap_or_else(|| "invalid command-line arguments".to_string())
}

/// Pull the `Usage: …` line out of clap's rendered error, prefix stripped.
fn clap_usage(err: &clap::Error) -> Option<String> {
    err.to_string()
        .lines()
        .find_map(|l| l.trim().strip_prefix("Usage: "))
        .map(str::to_string)
}

fn run(cli: &Cli) -> Result<Value> {
    let workspace = cli.workspace.canonicalize().unwrap_or(cli.workspace.clone());
    let mut client =
        Client::start(&cli.server_cmd, &workspace, Duration::from_secs(cli.timeout), cli.debug)?;

    let output: Value = match &cli.command {
        Command::Diagnostics { file, raw } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, language_id(&abs))?;
            let diagnostics = client.diagnostics(&uri)?;
            let items = diagnostics.as_array().cloned().unwrap_or_default();
            let rendered = render_diagnostics(&workspace, &abs, &items);
            let mut out = json!({"file": file, "count": rendered.len(), "diagnostics": rendered});
            if *raw {
                out["raw"] = diagnostics;
            }
            out
        }
        Command::Rename { file, target, new_name, apply, raw } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, language_id(&abs))?;
            let (line, character, resolved_from) = locate(&mut client, &uri, &abs, file, target)?;
            let edit = client.rename(&uri, line, character, new_name)?;
            // Build the summary from the pre-apply file contents, then apply.
            let result =
                render_rename_result(&workspace, &edit, file, target, new_name, line, resolved_from, *apply, *raw)?;
            if *apply {
                apply::apply_workspace_edit(&edit)?;
            }
            result
        }
        Command::References { file, target, raw } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, language_id(&abs))?;
            let (line, character, resolved_from) = locate(&mut client, &uri, &abs, file, target)?;
            let locations = client.references(&uri, line, character)?;
            let references = render_locations(&workspace, &locations);
            let mut out = json!({
                "target": target,
                "resolved": {"file": file, "line": line + 1, "from": resolved_from},
                "count": references.len(),
                "references": references,
            });
            if *raw {
                out["locations"] = json!(locations);
            }
            out
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

/// A WorkspaceEdit → an agent-legible rename result: success/fail status, the
/// scope of the change (how many files/edits), and a before/after row per changed
/// line. This is the output-side impedance transformer — the caller never has to
/// parse ranges or UTF-16 columns out of a raw WorkspaceEdit. The raw edit is
/// included under `edit` only when `raw` is set.
#[allow(clippy::too_many_arguments)]
fn render_rename_result(
    workspace: &Path,
    edit: &Value,
    file: &str,
    target: &str,
    new_name: &str,
    resolved_line: usize,
    resolved_from: &str,
    applied: bool,
    raw: bool,
) -> Result<Value> {
    let (files, edits, mut rows) = apply::summarize_workspace_edit(edit)?;
    rows.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    let changes: Vec<Value> = rows
        .iter()
        .map(|r| {
            let rel = r.path.strip_prefix(workspace).unwrap_or(&r.path).display().to_string();
            // Lines 1-indexed for display (editors count from 1); the before/after
            // text carries the change, so no column is exposed.
            json!({"file": rel, "line": r.line + 1, "before": r.before, "after": r.after})
        })
        .collect();
    let mut out = json!({
        "status": if applied { "applied" } else { "preview" },
        "target": target,
        "renamed_to": new_name,
        "resolved": {"file": file, "line": resolved_line + 1, "from": resolved_from},
        "scope": {"files": files, "edits": edits},
        "changes": changes,
    });
    if raw {
        out["edit"] = edit.clone();
    }
    Ok(out)
}

/// LSP Locations → `{file, line, text}` rows: paths relative to the workspace,
/// lines 1-indexed, and `text` the (trimmed) source line, so an agent reads the
/// result without reopening files — and never sees a UTF-16 column. (The column
/// is kept only as a sort key; `--raw` exposes the protocol Locations.)
fn render_locations(workspace: &Path, locations: &[Value]) -> Vec<Value> {
    let mut texts: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut rows: Vec<(String, usize, usize, String)> = Vec::new();
    for loc in locations {
        let uri = loc["uri"].as_str().unwrap_or_default();
        let path = apply::uri_to_path(uri);
        let line = loc["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
        let character = loc["range"]["start"]["character"].as_u64().unwrap_or(0) as usize;
        let lines = texts.entry(path.clone()).or_insert_with(|| {
            std::fs::read_to_string(&path)
                .map(|t| t.lines().map(|l| l.trim_end().to_string()).collect())
                .unwrap_or_default()
        });
        let text = lines.get(line).cloned().unwrap_or_default();
        let rel = path.strip_prefix(workspace).unwrap_or(&path).display().to_string();
        rows.push((rel, line, character, text));
    }
    rows.sort_by(|a, b| (a.0.as_str(), a.1, a.2).cmp(&(b.0.as_str(), b.1, b.2)));
    rows.into_iter()
        .map(|(file, line, _col, text)| json!({"file": file, "line": line + 1, "text": text}))
        .collect()
}

/// Raw LSP diagnostics → an agent-legible list: severity as a word, 1-indexed
/// line, the source line, optional code/source, and compact related-location
/// context — no protocol ranges or UTF-16 columns. The raw diagnostics stay
/// behind `--raw`.
fn render_diagnostics(workspace: &Path, abs: &Path, diagnostics: &[Value]) -> Vec<Value> {
    let lines: Vec<String> = std::fs::read_to_string(abs)
        .map(|t| t.lines().map(|l| l.trim_end().to_string()).collect())
        .unwrap_or_default();
    diagnostics
        .iter()
        .map(|d| {
            let line0 = d["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
            let mut row = json!({
                "severity": severity_word(d["severity"].as_u64()),
                "line": line0 + 1,
                "message": d["message"].as_str().unwrap_or_default(),
                "text": lines.get(line0).cloned().unwrap_or_default(),
            });
            match &d["code"] {
                Value::String(s) => row["code"] = json!(s),
                Value::Number(n) => row["code"] = json!(n),
                _ => {}
            }
            if let Some(src) = d["source"].as_str() {
                row["source"] = json!(src);
            }
            let related: Vec<Value> = d["relatedInformation"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|r| {
                            let path = apply::uri_to_path(r["location"]["uri"].as_str().unwrap_or_default());
                            let rel =
                                path.strip_prefix(workspace).unwrap_or(&path).display().to_string();
                            let rl = r["location"]["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
                            json!({"file": rel, "line": rl + 1, "message": r["message"].as_str().unwrap_or_default()})
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !related.is_empty() {
                row["related"] = json!(related);
            }
            row
        })
        .collect()
}

/// LSP DiagnosticSeverity (1–4) → a word an agent can branch on.
fn severity_word(severity: Option<u64>) -> &'static str {
    match severity {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "information",
        Some(4) => "hint",
        _ => "unknown",
    }
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
