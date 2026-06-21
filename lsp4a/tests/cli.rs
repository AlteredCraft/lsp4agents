//! End-to-end tests for the `lsp4a` CLI contract — `rename`, `references`, and
//! `diagnostics`.
//!
//! These run the *built* binary against throwaway fixture workspaces and drive a
//! real `ty` — they assert the agent-facing contract (the structured rename
//! presentation, decoy filtering, the on-disk effect of `--apply`, the
//! structured error envelopes, and that `references`/`diagnostics` speak in
//! 1-indexed lines + source text, never UTF-16 columns), which unit tests on the
//! pure helpers can't reach. This is the integration suite planning.md flags as a
//! prerequisite before the invasive protocol work (capability negotiation, a
//! second server).
//!
//! `ty` must be on PATH (the BYO model); if it's absent the tests skip rather
//! than fail, so a checkout without ty installed is still green.

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

#[test]
fn references_speak_in_lines_and_text_not_columns() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("references", &sample_files());
    let (ok, json) = run(&ws, &ty, &["references", "sample.py", "greet"]);
    assert!(ok, "expected exit 0, got {json}");

    assert_eq!(json["target"], "greet");
    assert_eq!(json["resolved"], serde_json::json!({"file": "sample.py", "line": 1, "from": "symbol"}));
    assert_eq!(json["count"], 4);
    assert!(json.get("locations").is_none(), "raw Locations must be behind --raw");

    let refs = json["references"].as_array().unwrap();
    assert_eq!(refs.len(), 4);
    for r in refs {
        // Agent-appropriate: 1-indexed line + source text, never a UTF-16 column.
        assert!(r.get("character").is_none(), "references must not leak columns");
        assert!(r["line"].as_u64().unwrap() >= 1, "lines are 1-indexed");
        assert!(r["text"].is_string());
    }
    // The declaration, found across the import boundary.
    assert!(refs.iter().any(|r| r["file"] == "sample.py" && r["line"] == 1
        && r["text"] == "def greet(name: str) -> str:"));
    assert!(refs.iter().any(|r| r["file"] == "consumer.py"
        && r["text"] == "from sample import greet"));
}

#[test]
fn references_raw_exposes_the_protocol_locations() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("references-raw", &sample_files());
    let (ok, json) = run(&ws, &ty, &["references", "sample.py", "greet", "--raw"]);
    assert!(ok, "expected exit 0, got {json}");
    let locs = json["locations"].as_array().unwrap();
    assert_eq!(locs.len(), 4);
    // The raw form keeps the protocol shape (ranges with UTF-16 columns).
    assert!(locs[0]["range"]["start"]["character"].is_u64());
}

#[test]
fn diagnostics_are_agent_legible() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("diagnostics", &sample_files());
    let (ok, json) = run(&ws, &ty, &["diagnostics", "sample.py"]);
    assert!(ok, "expected exit 0, got {json}");

    assert_eq!(json["count"], 1, "the deliberate greet(123) type error");
    assert!(json.get("raw").is_none(), "raw diagnostics must be behind --raw");

    let d = &json["diagnostics"][0];
    assert_eq!(d["severity"], "error", "severity is a word, not an int");
    assert_eq!(d["line"], 6, "1-indexed line of `message = greet(123)`");
    assert_eq!(d["code"], "invalid-argument-type");
    assert_eq!(d["source"], "ty");
    assert!(d["text"].as_str().unwrap().contains("greet(123)"), "the offending source line");
    assert!(d.get("range").is_none(), "no protocol ranges in the rendered diagnostic");
    // Related-location context, also in 1-indexed lines.
    let related = d["related"].as_array().unwrap();
    assert!(!related.is_empty());
    assert!(related.iter().all(|r| r["line"].as_u64().unwrap() >= 1 && r["file"].is_string()));
}

#[test]
fn diagnostics_raw_exposes_the_protocol_diagnostics() {
    let Some(ty) = ty_server_cmd() else {
        eprintln!("skipping: ty not on PATH (install: curl -LsSf https://astral.sh/ty/install.sh | sh)");
        return;
    };
    let ws = workspace("diagnostics-raw", &sample_files());
    let (ok, json) = run(&ws, &ty, &["diagnostics", "sample.py", "--raw"]);
    assert!(ok, "expected exit 0, got {json}");
    let raw = json["raw"].as_array().unwrap();
    assert_eq!(raw.len(), 1);
    assert!(raw[0]["range"]["start"]["line"].is_u64(), "raw keeps the protocol range");
}

// --- Usage errors (no ty needed: clap fails before any server is spawned) ---

#[test]
fn usage_errors_are_json_on_stdout_not_clap_prose() {
    let out = Command::new(env!("CARGO_BIN_EXE_lsp4a"))
        .arg("rename") // missing <FILE> <TARGET> <NEW_NAME>
        .output()
        .expect("spawn lsp4a");
    assert_eq!(out.status.code(), Some(2), "usage errors exit 2 (distinct from runtime's 1)");
    assert!(out.stderr.is_empty(), "no prose on stderr — the error is JSON on stdout");
    let json: Value =
        serde_json::from_slice(&out.stdout).expect("usage error must be JSON on stdout");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("missing required argument"));
    assert_eq!(json["error"]["usage"], "lsp4a rename <FILE> <TARGET> <NEW_NAME>");
}

#[test]
fn missing_subcommand_is_a_structured_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_lsp4a")).output().expect("spawn lsp4a");
    assert_eq!(out.status.code(), Some(2));
    let json: Value = serde_json::from_slice(&out.stdout).expect("must be JSON on stdout");
    assert!(json["error"]["message"].as_str().unwrap().contains("subcommand is required"));
}

#[test]
fn help_and_version_are_not_errors() {
    for flag in ["--help", "--version"] {
        let out = Command::new(env!("CARGO_BIN_EXE_lsp4a")).arg(flag).output().expect("spawn lsp4a");
        assert!(out.status.success(), "{flag} should exit 0");
        assert!(!out.stdout.is_empty(), "{flag} prints to stdout");
    }
}
