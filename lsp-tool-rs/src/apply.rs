//! Apply an LSP `WorkspaceEdit` to files on disk.
//!
//! Port of the Python testbed's applier, with the same two correctness traps
//! handled: UTF-16 offset conversion (LSP `character` counts UTF-16 code units,
//! but Rust `String` is UTF-8 / byte-indexed) and bottom-to-top application
//! (so an earlier splice never shifts a later edit's offsets).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Position {
    line: usize,
    character: usize,
}

#[derive(Deserialize)]
struct Range {
    start: Position,
    end: Position,
}

#[derive(Deserialize)]
struct TextEdit {
    range: Range,
    #[serde(rename = "newText")]
    new_text: String,
}

/// Byte offset in `text` where each line begins (LSP line numbering).
/// `\r\n`, `\r`, and `\n` each start a new line. Scanning by byte is safe: the
/// terminators are ASCII and never appear inside a multi-byte UTF-8 sequence.
fn line_starts(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                i += if i + 1 < bytes.len() && bytes[i + 1] == b'\n' { 2 } else { 1 };
                offsets.push(i);
            }
            b'\n' => {
                i += 1;
                offsets.push(i);
            }
            _ => i += 1,
        }
    }
    offsets
}

/// Resolve an LSP position (line, UTF-16 column) to a byte offset in `text`.
fn position_to_byte_offset(text: &str, starts: &[usize], pos: &Position) -> usize {
    if pos.line >= starts.len() {
        return text.len();
    }
    let line_start = starts[pos.line];
    let line_end = starts.get(pos.line + 1).copied().unwrap_or(text.len());
    let line = &text[line_start..line_end];

    let mut units = 0usize;
    for (byte_off, ch) in line.char_indices() {
        if units >= pos.character {
            return line_start + byte_off;
        }
        units += ch.len_utf16();
    }
    line_start + line.len()
}

/// Apply a list of TextEdits to `text` and return the new text. Pure (no I/O).
fn apply_text_edits(text: &str, edits: &[TextEdit]) -> String {
    let starts = line_starts(text);
    let mut resolved: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            (
                position_to_byte_offset(text, &starts, &e.range.start),
                position_to_byte_offset(text, &starts, &e.range.end),
                e.new_text.as_str(),
            )
        })
        .collect();
    // Bottom-to-top: earlier splices must not shift later offsets.
    resolved.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = text.to_string();
    for (start, end, new_text) in resolved {
        out.replace_range(start..end, new_text);
    }
    out
}

/// Turn a `file://` URI into a local path. %XX escapes decode to *bytes* and
/// the result is parsed as UTF-8 — pushing each byte as a `char` would mangle
/// multi-byte sequences (e.g. `%C3%A9` → `Ã©` instead of `é`).
pub fn uri_to_path(uri: &str) -> PathBuf {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let mut decoded: Vec<u8> = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                decoded.push(b);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
}

/// Flatten a WorkspaceEdit to `{uri: [TextEdit]}`, from either encoding.
/// Refuses Create/Rename/Delete resource ops rather than silently dropping them.
fn collect_text_edits(edit: &Value) -> Result<HashMap<String, Vec<TextEdit>>> {
    if let Some(doc_changes) = edit.get("documentChanges") {
        let arr = doc_changes.as_array().context("documentChanges not an array")?;
        let mut out: HashMap<String, Vec<TextEdit>> = HashMap::new();
        for change in arr {
            if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                bail!("WorkspaceEdit resource operation {kind:?} not supported in v0");
            }
            let uri = change["textDocument"]["uri"]
                .as_str()
                .context("documentChange missing textDocument.uri")?
                .to_string();
            let edits: Vec<TextEdit> = serde_json::from_value(change["edits"].clone())?;
            out.entry(uri).or_default().extend(edits);
        }
        return Ok(out);
    }
    if let Some(changes) = edit.get("changes") {
        let map: HashMap<String, Vec<TextEdit>> = serde_json::from_value(changes.clone())?;
        return Ok(map);
    }
    Ok(HashMap::new())
}

/// Apply a WorkspaceEdit to files on disk. Returns the list of changed paths.
pub fn apply_workspace_edit(edit: &Value) -> Result<Vec<String>> {
    let mut changed = Vec::new();
    for (uri, edits) in collect_text_edits(edit)? {
        let path = uri_to_path(&uri);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let new_text = apply_text_edits(&text, &edits);
        fs::write(&path, new_text).with_context(|| format!("writing {}", path.display()))?;
        changed.push(path.display().to_string());
    }
    changed.sort();
    Ok(changed)
}
