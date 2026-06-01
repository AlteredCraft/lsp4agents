//! lsp-tool (v0) — a stateless, hand-rolled LSP CLI that drives ty.
//!
//! Subcommands emit JSON on stdout (the server's own logs go to stderr):
//!   lsp-tool diagnostics <file>
//!   lsp-tool rename <file> <line> <character> <new-name> [--apply]
//!
//! Positions are zero-indexed; `character` is a UTF-16 column (ty uses utf-16).
//! v0 assumes utf-16; a real tool would read each server's negotiated encoding.

mod apply;
mod client;
mod transport;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use client::Client;

#[derive(Parser)]
#[command(name = "lsp-tool", version, about = "Stateless LSP rename/diagnostics CLI (v0, ty)")]
struct Cli {
    /// Workspace root (rootUri). File paths are resolved against it.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    /// Language-server command (whitespace-split).
    #[arg(long, default_value = "uv run ty server")]
    server_cmd: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report diagnostics for a file.
    Diagnostics {
        file: String,
    },
    /// Rename the symbol at a position; prints the WorkspaceEdit (or applies it).
    Rename {
        file: String,
        line: usize,
        character: usize,
        new_name: String,
        /// Apply the edit to disk instead of just printing it.
        #[arg(long)]
        apply: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace = cli.workspace.canonicalize().unwrap_or(cli.workspace.clone());

    let mut client = Client::start(&cli.server_cmd, &workspace)?;

    let output: Value = match &cli.command {
        Command::Diagnostics { file } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, "python")?;
            let diagnostics = client.diagnostics(&uri)?;
            json!({"file": file, "diagnostics": diagnostics})
        }
        Command::Rename { file, line, character, new_name, apply } => {
            let abs = client::resolve(&workspace, file)?;
            let uri = client.did_open(&abs, "python")?;
            let edit = client.rename(&uri, *line, *character, new_name)?;
            if *apply {
                let changed = apply::apply_workspace_edit(&edit)?;
                json!({"applied": true, "files_changed": changed, "edit": edit})
            } else {
                json!({"applied": false, "edit": edit})
            }
        }
    };

    client.shutdown()?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
