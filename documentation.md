# Architecture & LSP reference

The decided architecture of `lsp4a` and the LSP protocol substrate it rests
on. For *why* each choice was made, see [research.md](./research.md); for
*what's next*, see [planning.md](./planning.md). The implementation is in
[`lsp4a/`](./lsp4a/).

All spec links point at **LSP 3.17**:
<https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/>

---

## Architecture (decided)

`lsp4a` is a small **stateless Rust CLI** an LLM agent harness shells out to
for semantic code operations — today `rename`, `references`, and `diagnostics`,
JSON on stdout. The settled shape:

- **Semantic verbs, not raw LSP.** Callers give a symbol name and an intent —
  `rename sample.py greet salutation` — and the tool resolves the name to LSP
  coordinates (lexical scan → `prepareRename` verification → `references`
  dedupe; see [research.md](./research.md) § "the v0 interface leaked
  positions"), drives the server, and applies the edit. The LLM never counts a
  column. Ambiguity (same name, different scopes) is a structured error
  listing each candidate, with an explicit `line:char` target as the escape
  hatch.
- **Division of labor.** The caller (LLM) decides *what* to change and *to what*;
  the LSP decides *where* every reference is; the tool *applies* the resulting
  `WorkspaceEdit`. The LLM never synthesizes an edit list.
- **One server per language, as a subprocess.** The tool spawns the language
  server and speaks LSP over stdio — the language-agnostic seam (ty is Rust,
  gopls is Go; the client doesn't care). Servers are bring-your-own on a known
  path; the tool fails fast with an install hint if one is missing.
- **Stateless lifecycle.** Born → handshake → one operation → die. No daemon and
  no buffer sync (each call `didOpen`s disk-truth). A warm daemon is a future
  escalation if cold-start ever dominates.
- **Apply pipeline.** Offsets are converted from the server's negotiated
  `positionEncoding` (utf-16 for ty); edits apply **bottom-to-top**
  ([§5](#5-positions-and-the-utf-16-gotcha), [§7](#7-the-rename-workflow-the-headline));
  both `changes` and `documentChanges` encodings are handled, resource ops included.
- **Single-repo scope, workspace-wide reach.** Only what's statically
  resolvable in the workspace; dynamic/string refs and cross-repo consumers are
  out of scope. Within it, every operation is workspace-wide: the `<file>`
  argument only anchors *which symbol is meant* — renaming from a caller's
  file is identical to renaming from the definition's.
- **Agent-shaped failure.** Every wait on the server carries a `--timeout`
  (default 30s), and all failures — wedged server, unknown symbol, ambiguity —
  come back as `{"error": {"message", "data"?}}` on stdout with exit 1. An
  agent never parses a panic off stderr or hangs on a dead subprocess.
- **Harness integration.** A thin in-process tool shells out to the CLI;
  diagnostics ride a `post_edit` hook that aggregates type-checker + linter (the
  two have different jobs — ship both).

**Implementation:** Rust, in [`lsp4a/`](./lsp4a/).

**Language servers (bring-your-own).** Pin a version per language for
reproducibility.

| lang | server | acquire | default command |
|---|---|---|---|
| **Python** | **ty** | **`curl -LsSf https://astral.sh/ty/install.sh \| sh`** (standalone binary, no Python) | **`ty server`** |
| Go | gopls | `go install golang.org/x/tools/gopls@latest` | `gopls` |
| Rust | rust-analyzer | `rustup component add rust-analyzer` | `rust-analyzer` |
| TS | typescript-language-server | `npm i -g typescript-language-server` | `typescript-language-server --stdio` |

The rest of this document is the LSP protocol substrate the above rests on.

## 1. What problem LSP solves

Without it, supporting *M* editors × *N* languages is *M×N* bespoke
integrations. LSP makes it *M+N*: each editor speaks one protocol, each language
ships one server. The protocol is **JSON-RPC 2.0** over a stream — here, a
subprocess's stdio.

## 2. Transport: framing

Messages are length-prefixed, HTTP-style — *not* line-delimited (the JSON body
can contain newlines, so you must read exactly `Content-Length` bytes).

```
Content-Length: 119\r\n        ← ASCII header(s)
\r\n                           ← blank line ends the header block
{"jsonrpc":"2.0","id":1,...}   ← exactly N bytes of UTF-8 JSON
```

Spec: [Base Protocol](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#baseProtocol).

## 3. The three message shapes (the primitives)

```
Request       A ──{id, method, params}──▶ B     needs a reply
Response      A ◀──────{id, result}────── B     echoes the request's id
Notification  A ──{method, params}──────▶ B     no id, fire-and-forget
```

Two things that trip people up:

- **The dividing line is "does the sender need an answer?"** — not severity.
  "You have a type error" needs no reply, so it's a *notification*
  (`publishDiagnostics`). "Please apply this edit" needs a yes/no, so it's a
  *request* (`workspace/applyEdit`).
- **It's bidirectional.** Both peers can send requests; each maintains its
  **own independent `id` space**. A client's `id: 4` and a server's `id: 4` are
  unrelated. An `id` is just a correlation token, scoped to one sender within
  one session — it identifies nothing persistent.

Spec: [Request](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#requestMessage)
· [Response](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#responseMessage)
· [Notification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#notificationMessage).

## 4. Session lifecycle

```
client                          server
  │── initialize ──────────────▶│   request  (declares client capabilities)
  │◀───── capabilities ─────────│   response (server's menu of features)
  │── initialized ─────────────▶│   notification — "ready"; server may now push
  │                             │
  │── didOpen / didChange ─────▶│   notifications (client owns buffer state)
  │◀──── publishDiagnostics ────│   notification (pushed, unprompted)
  │── hover / definition / … ──▶│   requests
  │◀──────── results ───────────│   responses
  │                             │
  │── shutdown ────────────────▶│   request  → server stops work, stays alive
  │◀───────── null ─────────────│   response
  │── exit ────────────────────▶│   notification → process terminates
```

Before `initialized`, the server must not send unsolicited messages. The
client is the source of truth for buffer state — servers do **not** read files
from disk; `didOpen`/`didChange` hand them the text.

Spec: [Lifecycle Messages](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#lifeCycleMessages)
· [`initialize`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#initialize)
· [Document Synchronization](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_didOpen).

## 5. Positions and the UTF-16 gotcha

Positions are `{line, character}`, **zero-indexed**. By default `character`
counts **UTF-16 code units**, not bytes or code points. Equal for ASCII;
divergent for astral-plane chars (an emoji is 1 code point but 2 UTF-16 units).
The encoding is negotiated at `initialize` (`positionEncoding`); ty here uses
`utf-16`. Any tool that applies edits must convert offsets accordingly.

Spec: [`Position`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#position)
· [`PositionEncodingKind`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#positionEncodingKind).

## 6. Diagnostics: push vs pull

- **Push** — `textDocument/publishDiagnostics` (notification, server's timing).
- **Pull** — `textDocument/diagnostic` (a *request*, client's timing; LSP 3.17).
  ty advertises this via `diagnosticProvider` in its capabilities.

For a tool that edits then checks "did I break anything?", **pull** is easier to
orchestrate — the response corresponds exactly to your edit.

Spec: [publishDiagnostics](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_publishDiagnostics)
· [pull diagnostics](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_diagnostic).

## 7. The rename workflow (the headline)

```
client                                    server
  │── prepareRename {pos} ───────────────▶│  "renameable? what span?"
  │◀──── Range | null ────────────────────│  (null ⇒ bail)
  │── rename {pos, newName} ─────────────▶│  "compute every edit"
  │◀──── WorkspaceEdit ────────────────────│  {changes | documentChanges}
  │                                        │
  └─▶ apply WorkspaceEdit to disk          │  UTF-16 offsets; edits bottom-to-top

inverse path — server-driven edits (e.g. a code-action fix):
  │◀──── workspace/applyEdit {edit} ───────│  server REQUESTS the client apply
  │───── {applied: true} ─────────────────▶│
```

Notes for implementers:

- **`prepareRename`** returns one of four shapes: bare `Range`,
  `{range, placeholder}`, `{defaultBehavior: true}`, or `null`. Handle all.
- **`WorkspaceEdit`** has two encodings: the plain `changes` map (`{uri:
  TextEdit[]}`), or `documentChanges` (versioned files + Create/Rename/Delete
  resource ops). Prefer `documentChanges` when present.
- **Applying edits:** convert UTF-16 offsets to code-point indices, and apply
  **bottom-to-top** (descending by start) so earlier splices don't shift later
  offsets. Edits within one document don't overlap (spec guarantee).

Spec: [`prepareRename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_prepareRename)
· [`rename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_rename)
· [`WorkspaceEdit`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspaceEdit)
· [`workspace/applyEdit`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspace_applyEdit).

## 8. Why LSP rename beats grep — and its blind spot

LSP rename is **semantic**: the server resolved names against scopes and the
import graph, so it edits the *symbol's* references and ignores look-alikes in
strings, comments, or unrelated same-named symbols. (We proved this in the
testbed — the decoy `greet` in a comment and a string were left untouched.)

The tradeoff is precision vs recall:

| | over-matches | under-matches |
|---|---|---|
| **grep** | strings, comments, unrelated symbols | — |
| **LSP** | — | dynamic refs (`getattr`, string keys), cross-repo consumers |

The LSP only sees what's **statically resolvable within the workspace**. This
repo scopes the goal to **single-repo refactors**, where that boundary is
acceptable. A useful hybrid: do the precise LSP rename, then `grep` the *old*
name and surface the residue as "review these" rather than auto-editing it.

**Division of labor for an LLM tool:** the LLM decides *what* to rename and *to
what* (judgment); the LSP decides *where* every reference is (exhaustive,
deterministic). The LLM should *apply* the returned `WorkspaceEdit`, never
synthesize the edit list.

## Glossary

- **Symbol** — a named, resolvable entity (function, class, variable, parameter,
  module). Rename, references, and definition operate on *symbols*, not text.
- **Position** — `{line, character}`, zero-indexed; `character` is in UTF-16
  code units by default (see §5).
- **Range** — a `{start, end}` pair of Positions: a half-open span of text.
- **TextEdit** — `{range, newText}`: replace the text in `range` with `newText`.
- **WorkspaceEdit** — a bundle of TextEdits across one or more files (via
  `changes` or `documentChanges`); what a rename or code action returns.
- **Diagnostic** — a problem for a range (error/warning/hint) with a `severity`,
  `code`, and `source` (e.g. `"ty"`).
- **Capability** — a feature each side declares at `initialize`: the client
  states what it supports; the server advertises `*Provider` keys in return.
- **Provider** — a server capability key (`hoverProvider`, `renameProvider`, …)
  meaning "I can answer this method."
- **Notification** — an `id`-less, reply-less message (e.g. `publishDiagnostics`).
- **Request / Response** — an `id`-carrying message and its matching reply.
- **Document synchronization** — keeping the server's in-memory buffer current
  via `didOpen` / `didChange` / `didClose`; the client (not disk) is the source
  of truth.
- **URI** — files are addressed by `file://` URIs, not bare paths.
- **positionEncoding** — the negotiated unit for `character` offsets: `utf-16`
  (default), `utf-8`, or `utf-32`.

## References

- [LSP 3.17 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
- [JSON-RPC 2.0](https://www.jsonrpc.org/specification)
- [ty docs](https://docs.astral.sh/ty/)
- This repo: [README.md](./README.md) · [`lsp4a/`](./lsp4a/)
