# lsp-testbed

A scratch project for learning the Language Server Protocol by talking to it
directly — no editor, no client library, just stdio and JSON. Currently
points at [ty](https://docs.astral.sh/ty/) (Astral's Rust-based Python type
checker, which ships with an LSP server).

> **New to LSP?** Start with [`documentation.md`](./documentation.md) — protocol
> concepts (message shapes, lifecycle, the rename workflow, diagnostics) with
> diagrams and spec links. This README is about the *script*: how to run it and
> the exact session it drives.

## Setup

```bash
uv sync
uv run python lsp_raw_client.py   # drive a session, print every frame
uv run pytest                     # test the WorkspaceEdit apply logic
```

You'll get a long stream of `→ SEND` and `← RECV` blocks on stdout — those
are the actual JSON-RPC frames going to and from `ty server`. ty's own
log output appears on stderr, prefixed with `[ty stderr]`. The run ends with
`✎ APPLIED` blocks showing the files after the rename is applied (then
restored, so the run is repeatable).

## What `lsp_raw_client.py` does

The script is a from-scratch LSP client that drives `ty server` through a
minimal but realistic session and prints every wire-level message. The
intent is to make the protocol legible — not to be a useful client.

### Why "raw"?

LSP is just JSON-RPC 2.0 framed with `Content-Length` headers. Libraries
like `pygls`, `lsprotocol`, or VS Code's client hide that under typed
request/response helpers, which is great for building tools but bad for
learning what's actually on the wire. This script keeps the framing,
the lifecycle, and the message shapes all in one ~300-line file you can
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

### Files

- `documentation.md` — conceptual LSP notes (primitives, lifecycle, the
  rename workflow, push-vs-pull diagnostics) with diagrams and spec links.
  Start here for the big picture; this README covers the script specifically.
- `lsp_raw_client.py` — the client described above.
- `sample.py` — small program defining `greet`, with a deliberate type
  error (`greet(123)` where `greet` expects `str`) plus two decoy uses of
  the word "greet" (a comment and an f-string) that rename must ignore.
- `consumer.py` — imports and calls `greet` from `sample.py`, so a single
  rename produces edits in two files (the cross-file case).
- `test_apply.py` — tests for the WorkspaceEdit apply logic (UTF-16 offset
  conversion, bottom-to-top application, both edit encodings). `uv run pytest`.
- `pyproject.toml` / `uv.lock` — pin ty so the protocol output is
  reproducible across runs.

### References

[documentation.md](./documentation.md) carries the concept notes and the deep
links into the LSP 3.17 spec. External:
[JSON-RPC 2.0](https://www.jsonrpc.org/specification) ·
[ty docs](https://docs.astral.sh/ty/).
