# lsp4agents

Precise, semantic code operations — `rename` and `diagnostics` — for LLM coding
agents, backed by real Language Server Protocol servers instead of
`grep`-and-replace.

**The premise: LSP is editor-shaped; an agent is intent-shaped.** An editor has a
cursor, so a code position is free. An agent has only a symbol name and an
intent. The hard part — turning "rename `getUser`" into precise zero-indexed
UTF-16 coordinates and a verified, cross-file edit — is exactly what this tool
does, so the model never has to count columns (and quietly corrupt a string or
miss a reference).

> **Status: early, active research.** A working v0 (`lsp-tool`, a stateless Rust
> CLI) does `rename` + `diagnostics` against [ty](https://docs.astral.sh/ty/) for
> Python; Go, Rust, and TypeScript are the next targets. Built for the
> [Tilth](https://github.com/AlteredCraft/tilth) agent harness, but usable by any
> tool that can shell out.

## How we got here

It began as a *raw* LSP testbed — [`lsp_raw_client.py`](./lsp_raw_client.py), a
from-scratch client that speaks JSON-RPC over stdio and prints every frame, to
learn the protocol on the wire rather than through a library. That surfaced the
design (semantic verbs over a transparent proxy; the UTF-16 and apply-order
traps) and a build-vs-reuse question — settled in Rust's favor by a
Rust-vs-Python bake-off. Three docs carry the thinking:

- **[documentation.md](./documentation.md)** — the decided architecture plus an
  LSP protocol reference (diagrams, glossary, spec links).
- **[research.md](./research.md)** — the rationale, and the Rust-vs-Python
  comparison (behavior, lines-of-code-to-maintain, latency).
- **[planning.md](./planning.md)** — thin and forward-looking: what's next.

## Setup

```bash
uv sync   # installs ty into .venv/bin/ty, plus dev tools (pytest, ruff)
```

## The tool: `lsp-tool`

A stateless Rust CLI ([`lsp-tool-rs/`](./lsp-tool-rs/)) the harness shells out to
for `rename` and `diagnostics`, JSON on stdout. It spawns a language server as a
subprocess and speaks LSP over stdio — the language-agnostic seam (ty is Rust,
gopls is Go; the client doesn't care). For Python it drives ty directly
(`.venv/bin/ty`, no Python in the loop). An earlier Python-on-multilspy trial
lives in [`lsp-tool-py/`](./lsp-tool-py/), kept for the comparison that chose the
language — see [`research.md`](./research.md).

Run from the repo root (`--workspace` defaults to `.`):

```bash
# Rust — the implementation. First `cargo run` compiles, then it's instant.
cargo run --manifest-path lsp-tool-rs/Cargo.toml -- diagnostics sample.py
cargo run --manifest-path lsp-tool-rs/Cargo.toml -- rename sample.py 5 10 salutation

# Python — early trial, kept for the comparison.
uv run python lsp-tool-py/lsp_tool.py diagnostics sample.py
```

`rename` takes `<file> <line> <character> <new-name>` (0-indexed). Without
`--apply` it prints the `WorkspaceEdit`; add `--apply` to edit the files in place
(restore with `git checkout sample.py consumer.py`).

## The testbed: LSP on the wire

```bash
uv run python lsp_raw_client.py   # drive a session, print every frame
uv run pytest                     # test the WorkspaceEdit apply logic
```

You get a stream of `→ SEND` and `← RECV` blocks — the actual JSON-RPC frames to
and from `ty server` (ty's own logs go to stderr, prefixed `[ty stderr]`). The
run ends with `✎ APPLIED` blocks showing the files after a rename is applied,
then restores the originals so it stays repeatable. The script is a from-scratch
LSP client — framing, lifecycle, and message shapes in one ~450-line file you can
read top to bottom — built to make the protocol legible, not to be a useful
client. It's where the tool's design came from.

### Why "raw"?

LSP is just JSON-RPC 2.0 framed with `Content-Length` headers. Libraries
like `pygls`, `lsprotocol`, or VS Code's client hide that under typed
request/response helpers, which is great for building tools but bad for
learning what's actually on the wire. This script keeps the framing,
the lifecycle, and the message shapes all in one ~450-line file you can
read top to bottom.

### Transport

LSP over stdio is JSON-RPC framed with `Content-Length` headers — see
[documentation.md §2](./documentation.md#2-transport-framing) for the wire
format. In this repo the `Framer` class owns it: `send()` prefixes the JSON
with the header block; `recv()` reads headers until the blank line, then reads
**exactly** N bytes of body (never `readline()` on the body — the JSON can
contain newlines), decodes, and returns a dict.

### Message types

JSON-RPC has three shapes — request, response, notification — and this script
exercises all of them (`initialize`/`hover` are requests; `initialized`/
`didOpen`/`exit` are notifications; `publishDiagnostics` is one the server
pushes at *us*). See
[documentation.md §3](./documentation.md#3-the-three-message-shapes-the-primitives)
for what distinguishes them and the per-sender `id` rule.

### Lifecycle

Every session follows the same skeleton — `initialize` → `initialized` → work
→ `shutdown` → `exit`, diagrammed in
[documentation.md §4](./documentation.md#4-session-lifecycle). The next section
is that lifecycle made concrete: the exact frames this script sends and gets
back.

### The conversation the script actually has

1. **`initialize` (request, id=1).** Client announces who it is, where
   the workspace root is, and which features it supports. The response
   is the server's capabilities — the menu of methods it can answer.
   ty's response includes `hoverProvider`, `definitionProvider`,
   `renameProvider.prepareProvider`, `diagnosticProvider` with
   `interFileDependencies`, and a lot more. Worth reading carefully —
   this single response tells you everything you can ask ty to do.

2. **`initialized` (notification).** No body, no reply. Until the client
   sends this, the server is forbidden from sending unsolicited
   messages. Right after this notification the floodgates open.

3. **`textDocument/didOpen` (notification, ×2).** Hands ty the current
   text of `sample.py` **and** `consumer.py`. LSP servers do **not** read
   files from disk — the client is the source of truth for buffer state.
   Each open draws an unprompted `textDocument/publishDiagnostics`:
   `sample.py` reports one diagnostic (`invalid-argument-type`, with
   `relatedInformation` pointing at where `greet` and its parameter were
   declared); `consumer.py` reports an empty `diagnostics: []` — ty's way
   of saying "checked, clean." Both files must be open for the later
   rename to find references across them.

4. **`textDocument/hover` (request, id=2).** Asks "what's at line 5,
   col 10?". ty returns a markdown code block with the resolved
   signature: `def greet(name: str) -> str`. (Positions are zero-indexed
   `{line, character}`; see [documentation.md §5](./documentation.md#5-positions-and-the-utf-16-gotcha)
   on the UTF-16 encoding.)

5. **`textDocument/prepareRename` (request, id=3).** The "can I rename
   this, and what span?" probe. ty returns the `Range` of `greet` (line 5,
   chars 10–15) — the bare-`Range` form — or `null` if the position isn't
   renameable. ([documentation.md §7](./documentation.md#7-the-rename-workflow-the-headline)
   lists all four result shapes a robust client must handle.)

6. **`textDocument/rename` (request, id=4).** Sends the position plus
   `newName: "salutation"`; ty returns a `WorkspaceEdit` whose `changes`
   map carries **four** edits across **two** files — the `def greet` and
   `greet(123)` in `sample.py`, plus the `import greet` and `greet("world")`
   in `consumer.py`. It does **not** touch the word "greet" in `sample.py`'s
   decoy comment/f-string or `consumer.py`'s docstring — those aren't
   references to the symbol. This is the object an LLM should *apply*, never
   *synthesize*; the encodings and apply-order rules are in
   [documentation.md §7](./documentation.md#7-the-rename-workflow-the-headline).

7. **Apply the `WorkspaceEdit`.** `apply_workspace_edit()` writes the four
   edits to disk, completing the rename round-trip; the script prints the
   edited files (`✎ APPLIED`) then restores the originals so it stays
   re-runnable. See [Applying the WorkspaceEdit](#applying-the-workspaceedit).

8. **`shutdown` (request, id=5) then `exit` (notification).** Two-step
   so a client can confirm a clean stop (`shutdown` returns `null`)
   before the process actually goes away.

### Applying the WorkspaceEdit

`apply_workspace_edit()` turns the rename's `WorkspaceEdit` into file writes,
completing the round-trip; the pure `apply_text_edits(text, edits) -> str` does
the splicing and is the piece worth lifting into a real tool. It reads both
`changes` and `documentChanges` and refuses `Create`/`Rename`/`Delete` resource
ops loudly rather than dropping them. The two traps it handles — UTF-16 offset
conversion and bottom-to-top application — are explained in
[documentation.md §5](./documentation.md#5-positions-and-the-utf-16-gotcha) and
[§7](./documentation.md#7-the-rename-workflow-the-headline), and exercised by
`test_apply.py` (`uv run pytest`).

### `_wait_for_id` — why we can't just read the next frame

The server is allowed to interleave notifications between our request
and its response. In this run, the diagnostic notification arrives
*before* the hover response. `_wait_for_id` keeps reading frames until
it sees one whose `id` matches the request we sent.

### Extending the script

Each new request is one more `framer.send(...)` plus a `_wait_for_id`.
Good next experiments against the same files:

- **`textDocument/definition`** at position (5, 10) — should return
  the range of `def greet` on line 0.
- **`textDocument/references`** with `context: {includeDeclaration: true}`
  — should return every occurrence of `greet` across both open files.
- **`textDocument/codeAction`** over the diagnostic range — ty advertises
  `codeActionProvider` with a `quickfix` kind, so this is the path to
  "offer the fix for the type error," and the action's edit is again a
  `WorkspaceEdit`.
- **`textDocument/didChange`** to edit a buffer in memory (without
  touching disk), then re-request hover/diagnostics to watch results
  update — the core loop of an interactive client. This pairs naturally
  with `apply_text_edits` to keep the server's view and disk in sync.

## Files

- `documentation.md` — the decided architecture of `lsp-tool` plus an LSP
  protocol reference (message shapes, lifecycle, rename workflow, diagnostics)
  with diagrams, a glossary, and spec links.
- `research.md` — the rationale behind the design, and the Rust-vs-Python v0
  comparison (behavior, lines-of-code-to-maintain, latency).
- `planning.md` — thin and forward-looking: current reality and next steps.
- `lsp-tool-rs/` — **the tool**, in Rust: hand-rolled framing, a ty-driven
  `rename`/`diagnostics` CLI, and a UTF-16-aware applier. Build/run with `cargo`.
- `lsp-tool-py/` — an early Python-on-multilspy trial, superseded by the Rust
  implementation; kept for `research.md`.
- `lsp_raw_client.py` — the raw testbed client described above.
- `sample.py` — small program defining `greet`, with a deliberate type
  error (`greet(123)` where `greet` expects `str`) plus two decoy uses of
  the word "greet" (a comment and an f-string) that rename must ignore.
- `consumer.py` — imports and calls `greet` from `sample.py`, so a single
  rename produces edits in two files (the cross-file case).
- `test_apply.py` — tests for the WorkspaceEdit apply logic (UTF-16 offset
  conversion, bottom-to-top application, both edit encodings). `uv run pytest`.
- `pyproject.toml` / `uv.lock` — pin `ty` (protocol output), `multilspy` (the
  Python v0), and dev tools (`pytest`, `ruff`). The lockfile keeps runs reproducible.

## References

[documentation.md](./documentation.md) carries the concept notes and the deep
links into the LSP 3.17 spec. External:
[JSON-RPC 2.0](https://www.jsonrpc.org/specification) ·
[ty docs](https://docs.astral.sh/ty/).
