//! A minimal stateless LSP client: spawn a server, run the handshake, issue one
//! operation, shut down. This is the "born / do work / die" model from
//! planning.md — fine for a fast server like ty on a small repo.
//!
//! Frames are read on a dedicated thread and handed over an mpsc channel, so
//! every wait carries a timeout — a wedged server surfaces as a structured
//! error instead of hanging the (agent-facing) CLI forever.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::transport::{read_message, send_message};

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Result<Value>>,
    next_id: i64,
    timeout: Duration,
    /// Pushed diagnostics, keyed by document URI (from publishDiagnostics).
    pushed_diagnostics: HashMap<String, Value>,
    /// The server's `initialize` result `capabilities` object.
    pub capabilities: Value,
}

impl Client {
    /// Spawn `server_cmd` (whitespace-split) in `workspace` and run initialize +
    /// initialized. ty logs to stderr, which we inherit so JSON on stdout stays clean.
    pub fn start(server_cmd: &str, workspace: &Path, timeout: Duration) -> Result<Client> {
        let parts: Vec<&str> = server_cmd.split_whitespace().collect();
        let (cmd, args) = parts.split_first().context("empty --server-cmd")?;

        let mut child = Command::new(cmd)
            .args(args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| spawn_error(e, server_cmd, cmd))?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let incoming = spawn_reader(stdout);

        let mut client = Client {
            child,
            stdin,
            incoming,
            next_id: 0,
            timeout,
            pushed_diagnostics: HashMap::new(),
            capabilities: Value::Null,
        };

        let root_uri = path_to_uri(&workspace.canonicalize()?);
        let init_id = client.request(
            "initialize",
            json!({
                "processId": null,
                "clientInfo": {"name": "lsp4a", "version": "0.1.0"},
                "rootUri": root_uri,
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "publishDiagnostics": {"relatedInformation": true},
                        "rename": {"prepareSupport": true},
                        "references": {},
                        "diagnostic": {}
                    }
                }
            }),
        )?;
        let init = client.wait_for_id(init_id)?;
        client.capabilities = init.get("capabilities").cloned().unwrap_or(Value::Null);

        // The applier assumes utf-16 columns; refuse a server that negotiated
        // something else rather than silently corrupting offsets.
        if let Some(enc) = client.capabilities.get("positionEncoding").and_then(Value::as_str) {
            if enc != "utf-16" {
                bail!("server negotiated positionEncoding {enc:?}; only utf-16 is supported");
            }
        }

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
    /// interleave (notably publishDiagnostics) are stashed, not dropped; server→
    /// client *requests* get a stock reply (their id space is independent of
    /// ours, so they must never be mistaken for our response).
    pub fn wait_for_id(&mut self, id: i64) -> Result<Value> {
        loop {
            let msg = match self.incoming.recv_timeout(self.timeout) {
                Ok(frame) => frame?,
                Err(RecvTimeoutError::Timeout) => {
                    bail!("timed out after {:?} waiting for the server's response", self.timeout)
                }
                Err(RecvTimeoutError::Disconnected) => bail!("server exited mid-request"),
            };
            if let Some(method) = msg.get("method").and_then(Value::as_str) {
                if let Some(server_id) = msg.get("id") {
                    // Server→client request. Answer minimally so the server
                    // doesn't stall: per-item nulls for workspace/configuration,
                    // null for everything else.
                    let result = if method == "workspace/configuration" {
                        let n = msg["params"]["items"].as_array().map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; n])
                    } else {
                        Value::Null
                    };
                    send_message(
                        &mut self.stdin,
                        &json!({"jsonrpc": "2.0", "id": server_id, "result": result}),
                    )?;
                    continue;
                }
                if method == "textDocument/publishDiagnostics" {
                    if let Some(uri) = msg["params"]["uri"].as_str() {
                        self.pushed_diagnostics
                            .insert(uri.to_string(), msg["params"]["diagnostics"].clone());
                    }
                }
                continue; // other notifications: ignore
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

    /// textDocument/prepareRename — `Ok(None)` means "not renameable here".
    pub fn prepare_rename(&mut self, uri: &str, line: usize, character: usize) -> Result<Option<Value>> {
        let id = self.request(
            "textDocument/prepareRename",
            json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}),
        )?;
        match self.wait_for_id(id) {
            Ok(Value::Null) => Ok(None),
            Ok(v) => Ok(Some(v)),
            // Some servers error instead of returning null on non-renameable
            // positions; for candidate filtering that's the same answer.
            Err(_) => Ok(None),
        }
    }

    /// prepareRename then rename. Returns the WorkspaceEdit.
    pub fn rename(&mut self, uri: &str, line: usize, character: usize, new_name: &str) -> Result<Value> {
        if self.prepare_rename(uri, line, character)?.is_none() {
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

    /// textDocument/references (declaration included). Returns the Location list.
    pub fn references(&mut self, uri: &str, line: usize, character: usize) -> Result<Vec<Value>> {
        if self.capabilities.get("referencesProvider").map_or(true, Value::is_null) {
            bail!("server does not advertise referencesProvider");
        }
        let id = self.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character},
                "context": {"includeDeclaration": true}
            }),
        )?;
        match self.wait_for_id(id)? {
            Value::Array(locs) => Ok(locs),
            Value::Null => Ok(Vec::new()),
            other => bail!("unexpected references result: {other}"),
        }
    }

    /// Graceful shutdown + exit, with a kill fallback so a deaf server can't
    /// leave the CLI hanging.
    pub fn shutdown(mut self) -> Result<()> {
        let id = self.request("shutdown", Value::Null)?;
        let _ = self.wait_for_id(id);
        let _ = self.notify("exit", Value::Null);
        for _ in 0..40 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        Ok(())
    }
}

/// Turn a spawn failure into an actionable message. A missing server is the
/// common case (BYO model), so point at how to install ty rather than leaking a
/// raw errno; other failures keep their OS context.
fn spawn_error(e: std::io::Error, server_cmd: &str, cmd: &str) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "language server {cmd:?} not found. Install ty (a standalone binary, no Python) with:\n  \
             curl -LsSf https://astral.sh/ty/install.sh | sh\n\
             then make sure it's on PATH, or pass --server-cmd for a different server."
        )
    } else {
        anyhow::Error::new(e).context(format!("spawning language server: {server_cmd}"))
    }
}

/// Read frames off the server's stdout on a dedicated thread, so the main
/// thread can wait with a timeout. The thread ends when the pipe closes or the
/// Client is dropped.
fn spawn_reader(stdout: ChildStdout) -> Receiver<Result<Value>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_message(&mut reader) {
                Ok(msg) => {
                    if tx.send(Ok(msg)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });
    rx
}

/// `file://` URI for an absolute path, percent-encoding everything outside the
/// RFC 3986 unreserved set (plus `/`).
pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Resolve a workspace-relative or absolute file path to an absolute PathBuf.
pub fn resolve(workspace: &Path, file: &str) -> Result<PathBuf> {
    let p = workspace.join(file);
    p.canonicalize()
        .with_context(|| format!("file not found: {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encodes_spaces_and_non_ascii() {
        let uri = path_to_uri(Path::new("/tmp/a dir/héllo.py"));
        assert_eq!(uri, "file:///tmp/a%20dir/h%C3%A9llo.py");
    }
}
