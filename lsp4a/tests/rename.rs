//! End-to-end tests for the `rename` JSON contract.
//!
//! These run the *built* `lsp4a` binary against throwaway fixture workspaces and
//! drive a real `ty` — they assert the agent-facing contract (the structured
//! presentation, decoy filtering, the on-disk effect of `--apply`, and the
//! structured error envelopes), which unit tests on the pure helpers can't reach.
//! This is the integration suite planning.md flags as a prerequisite before the
//! invasive protocol work (capability negotiation, a second server).
//!
//! `ty` is the uv-managed binary at the repo root (`../.venv/bin/ty`); if it's
//! absent the tests skip rather than fail, so a checkout without `uv sync` is
//! still green.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// `ty server` if ty is on PATH, else `None` so the suite skips. ty is a
/// standalone binary (BYO model) — no Python, no `.venv`.
fn ty_server_cmd() -> Option<String> {
    let installed =
        Command::new("ty").arg("--version").output().is_ok_and(|o| o.status.success());
    installed.then(|| "ty server".to_string())
}

/// A fresh fixture workspace in the *system* temp dir. It must live outside the
/// repo tree on purpose: ty walks up looking for project config, so a workspace
/// nested under this repo's `pyproject.toml` gets analyzed as part of the outer
/// project and the fixture file is treated as out-of-project (prepareRename
/// returns null). `name` + pid keep concurrent tests/runs from colliding.
fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lsp4a-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (path, content) in files {
        std::fs::write(dir.join(path), content).unwrap();
    }
    dir
}

/// Run the built binary in `ws`; return `(success, parsed-stdout-json)`.
fn run(ws: &Path, server: &str, args: &[&str]) -> (bool, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_lsp4a"))
        .arg("--workspace")
        .arg(ws)
        .arg("--server-cmd")
        .arg(server)
        .args(args)
        .output()
        .expect("spawn lsp4a");
    let json = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("stdout was not JSON: {e}\n--- stdout ---\n{}", String::from_utf8_lossy(&out.stdout))
    });
    (out.status.success(), json)
}

/// sample.py + consumer.py: a function with comment/string decoys, imported
/// across a file boundary (the cross-file rename case).
const SAMPLE: &str = concat!(
    "def greet(name: str) -> str:\n",
    "    # Decoy: the word greet appears in this comment but must NOT be renamed.\n",
    "    return f\"Hello, {name}! The string also says greet on purpose.\"\n",
    "\n",
    "\n",
    "message = greet(123)\n",
    "print(message)\n",
);
const CONSUMER: &str = concat!(
    "from sample import greet\n",
    "\n",
    "reply = greet(\"world\")\n",
    "print(reply)\n",
);

/// shadow.py: two distinct locals both named `total`, in separate scopes.
const SHADOW: &str = concat!(
    "def a():\n",
    "    total = 1\n",
    "    return total\n",
    "\n",
    "def b():\n",
    "    total = 2\n",
    "    return total\n",
);

fn sample_files() -> Vec<(&'static str, &'static str)> {
    vec![("sample.py", SAMPLE), ("consumer.py", CONSUMER)]
}

#[test]
fn preview_is_a_structured_summary_not_a_workspace_edit() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("preview", &sample_files());
    let (ok, json) = run(&ws, &ty, &["rename", "sample.py", "greet", "salutation"]);
    assert!(ok, "expected exit 0, got {json}");

    assert_eq!(json["status"], "preview");
    assert_eq!(json["target"], "greet");
    assert_eq!(json["renamed_to"], "salutation");
    assert_eq!(json["resolved"], serde_json::json!({"file": "sample.py", "line": 1, "from": "symbol"}));
    assert_eq!(json["scope"], serde_json::json!({"files": 2, "edits": 4}));

    // Presentation-only by default: no raw WorkspaceEdit leaks into the output.
    assert!(json.get("edit").is_none(), "raw edit must be behind --raw");

    let changes = json["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 4, "4 changed lines across 2 files");
    // The before/after rows carry the change; no protocol coordinates (columns).
    for c in changes {
        assert!(c.get("character").is_none());
        assert!(c["line"].is_u64());
        assert_ne!(c["before"], c["after"]);
    }
    // The declaration line, transformed.
    assert!(changes.iter().any(|c| c["file"] == "sample.py"
        && c["after"] == "def salutation(name: str) -> str:"));
    // The decoy comment/string line is NOT among the changes.
    assert!(
        changes.iter().all(|c| !c["before"].as_str().unwrap().contains("Decoy")),
        "the comment/string decoy line must be filtered out"
    );

    // Preview must not touch disk.
    let on_disk = std::fs::read_to_string(ws.join("sample.py")).unwrap();
    assert!(on_disk.contains("def greet("), "preview must not write to disk");
}

#[test]
fn raw_flag_includes_the_workspace_edit() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("raw", &sample_files());
    let (ok, json) = run(&ws, &ty, &["rename", "sample.py", "greet", "salutation", "--raw"]);
    assert!(ok, "expected exit 0, got {json}");
    // The summary is still there, plus the raw edit for callers that apply it themselves.
    assert_eq!(json["scope"]["edits"], 4);
    let edit = &json["edit"];
    assert!(
        edit.get("changes").is_some() || edit.get("documentChanges").is_some(),
        "raw edit should carry one of the two WorkspaceEdit encodings, got {edit}"
    );
}

#[test]
fn apply_rewrites_disk_and_leaves_decoys_untouched() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("apply", &sample_files());
    let (ok, json) = run(&ws, &ty, &["rename", "sample.py", "greet", "salutation", "--apply"]);
    assert!(ok, "expected exit 0, got {json}");
    assert_eq!(json["status"], "applied");

    let sample = std::fs::read_to_string(ws.join("sample.py")).unwrap();
    let consumer = std::fs::read_to_string(ws.join("consumer.py")).unwrap();
    assert!(sample.contains("def salutation(name: str) -> str:"));
    assert!(sample.contains("message = salutation(123)"));
    assert!(consumer.contains("from sample import salutation"));
    assert!(consumer.contains("reply = salutation(\"world\")"));
    // Decoys: the comment and the string literal still say "greet".
    assert!(sample.contains("the word greet appears in this comment"));
    assert!(sample.contains("also says greet on purpose"));
}

#[test]
fn shadowed_symbol_is_a_structured_ambiguity_error() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("ambiguous", &[("shadow.py", SHADOW)]);
    let (ok, json) = run(&ws, &ty, &["rename", "shadow.py", "total", "sum"]);
    assert!(!ok, "ambiguous rename must exit non-zero, got {json}");
    let candidates = json["error"]["data"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 4, "both scopes' occurrences are listed");
    assert!(json["error"]["message"].as_str().unwrap().contains("ambiguous"));
}

#[test]
fn position_target_disambiguates_one_scope() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("position", &[("shadow.py", SHADOW)]);
    // `1:4` = the `total` in `a()` (line 1, col 4, zero-indexed) — the escape hatch.
    let (ok, json) = run(&ws, &ty, &["rename", "shadow.py", "1:4", "sum"]);
    assert!(ok, "expected exit 0, got {json}");
    assert_eq!(json["resolved"]["from"], "position");
    assert_eq!(json["scope"], serde_json::json!({"files": 1, "edits": 2}));
    // Only the first scope changed.
    let changes = json["changes"].as_array().unwrap();
    assert!(changes.iter().all(|c| c["after"].as_str().unwrap().contains("sum")));
}

#[test]
fn unknown_symbol_is_a_structured_error() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("unknown", &[("shadow.py", SHADOW)]);
    let (ok, json) = run(&ws, &ty, &["rename", "shadow.py", "nope", "sum"]);
    assert!(!ok, "unknown symbol must exit non-zero, got {json}");
    assert!(json["error"]["message"].as_str().unwrap().contains("not found"));
}
