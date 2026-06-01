//! A minimal stateless LSP client: spawn a server, run the handshake, issue one
//! operation, shut down. This is the "born / do work / die" model from
//! planning.md — fine for a fast server like ty on a small repo.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::transport::{read_message, send_message};

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
    /// Pushed diagnostics, keyed by document URI (from publishDiagnostics).
    pushed_diagnostics: HashMap<String, Value>,
}

impl Client {
    /// Spawn `server_cmd` (whitespace-split) in `workspace` and run initialize +
    /// initialized. ty logs to stderr, which we inherit so JSON on stdout stays clean.
    pub fn start(server_cmd: &str, workspace: &Path) -> Result<Client> {
        let parts: Vec<&str> = server_cmd.split_whitespace().collect();
        let (cmd, args) = parts.split_first().context("empty --server-cmd")?;

        let mut child = Command::new(cmd)
            .args(args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning language server: {server_cmd}"))?;

        let stdin = child.stdin.take().context("no stdin")?;
        let reader = BufReader::new(child.stdout.take().context("no stdout")?);

        let mut client = Client {
            child,
            stdin,
            reader,
            next_id: 0,
            pushed_diagnostics: HashMap::new(),
        };

        let root_uri = path_to_uri(&workspace.canonicalize()?);
        let init_id = client.request(
            "initialize",
            json!({
                "processId": null,
                "clientInfo": {"name": "lsp-tool", "version": "0.1.0"},
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "publishDiagnostics": {"relatedInformation": true},
                        "rename": {"prepareSupport": true},
                        "diagnostic": {}
                    }
                }
            }),
        )?;
        client.wait_for_id(init_id)?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    fn new_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// Send a request; returns its id. Does not wait for the response.
    pub fn request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.new_id();
        send_message(
            &mut self.stdin,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        send_message(
            &mut self.stdin,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
    }

    /// Drain frames until the response matching `id` arrives. Notifications that
    /// interleave (notably publishDiagnostics) are stashed, not dropped.
    pub fn wait_for_id(&mut self, id: i64) -> Result<Value> {
        loop {
            let msg = read_message(&mut self.reader)?;
            if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
                if let Some(uri) = msg["params"]["uri"].as_str() {
                    self.pushed_diagnostics
                        .insert(uri.to_string(), msg["params"]["diagnostics"].clone());
                }
                continue;
            }
            if msg.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    bail!("server returned error for id {id}: {err}");
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// textDocument/didOpen — hand the server the file's current text.
    pub fn did_open(&mut self, abs_path: &Path, language_id: &str) -> Result<String> {
        let uri = path_to_uri(abs_path);
        let text = std::fs::read_to_string(abs_path)
            .with_context(|| format!("reading {}", abs_path.display()))?;
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}),
        )?;
        Ok(uri)
    }

    /// Pull diagnostics (textDocument/diagnostic). Falls back to whatever the
    /// server pushed if pull returns nothing.
    pub fn diagnostics(&mut self, uri: &str) -> Result<Value> {
        let id = self.request(
            "textDocument/diagnostic",
            json!({"textDocument": {"uri": uri}}),
        )?;
        let report = self.wait_for_id(id)?;
        let items = report.get("items").cloned().unwrap_or(Value::Null);
        if items.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            return Ok(items);
        }
        // Pull was empty/unsupported — use the pushed set if present.
        Ok(self
            .pushed_diagnostics
            .get(uri)
            .cloned()
            .unwrap_or_else(|| items))
    }

    /// prepareRename then rename. Returns the WorkspaceEdit.
    pub fn rename(&mut self, uri: &str, line: usize, character: usize, new_name: &str) -> Result<Value> {
        let prep_id = self.request(
            "textDocument/prepareRename",
            json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}),
        )?;
        let prep = self.wait_for_id(prep_id)?;
        if prep.is_null() {
            bail!("position ({line},{character}) is not renameable (prepareRename returned null)");
        }
        let rename_id = self.request(
            "textDocument/rename",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character},
                "newName": new_name
            }),
        )?;
        self.wait_for_id(rename_id)
    }

    /// Graceful shutdown + exit.
    pub fn shutdown(mut self) -> Result<()> {
        let id = self.request("shutdown", Value::Null)?;
        let _ = self.wait_for_id(id);
        let _ = self.notify("exit", Value::Null);
        let _ = self.child.wait();
        Ok(())
    }
}

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

/// Resolve a workspace-relative or absolute file path to an absolute PathBuf.
pub fn resolve(workspace: &Path, file: &str) -> Result<PathBuf> {
    let p = workspace.join(file);
    p.canonicalize()
        .with_context(|| format!("file not found: {}", p.display()))
}
