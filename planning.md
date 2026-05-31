# Planning — an LSP-backed refactor tool for an agent harness

Where this testbed is heading. The runnable spike is
[`lsp_raw_client.py`](./lsp_raw_client.py); protocol mechanics live in
[documentation.md](./documentation.md). This file is the *why* and the *open
questions* behind building a real tool — an LSP-driven rename/refactor
capability for an LLM agent harness ([Tilth](https://github.com/AlteredCraft/tilth)).

Status tags: **[decided]** · **[leaning]** · **[open]**.

---

## The framing

> **LSP is editor-shaped; an agent is intent-shaped.** LSP was designed assuming
> a human with a cursor — the position is free, it's wherever the caret is. An
> LLM has no cursor. It has a symbol name and an intention. So the single
> hardest part of using LSP, resolving "the symbol `getUser`" to a precise
> zero-indexed UTF-16 (line, character), is exactly what a transparent proxy
> refuses to do — it dumps that on the LLM's arithmetic, which is the weakest
> link. The model will miscount a column, especially with the UTF-16 thing we
> hit, and you'll get a rename of the wrong span or a no-op.

Everything below follows from that.

## Core decisions

**Semantic API, not a transparent proxy. [decided]**
Expose intent verbs (`rename_symbol`, `diagnostics`, later maybe
`find_references`), not raw LSP methods. The tool resolves name→position (via
`documentSymbol` / `workspaceSymbol`), drives the LSP, and applies the edit —
the LLM never sees a coordinate.
- **Multi-language (py/ts/rust/go) strengthens this:** one normalized verb hides
  five servers' encoding/capability/init differences.
- **Weak open models strengthen this:** a deepseek-class model is the most
  likely to botch raw position arithmetic, so don't hand it positions.

**Division of labor. [decided]**
The LLM decides *what* to rename and *to what* (judgment). The LSP decides
*where* every reference is (exhaustive, deterministic). The tool *applies* the
returned `WorkspaceEdit`. The LLM never synthesizes an edit list.

**Single-repo scope. [decided]**
Reason only about what's statically resolvable within the workspace. Out of
scope: dynamic refs (`getattr`, string keys), cross-repo API consumers.
Mitigation (later): after a rename, `grep` the *old* name and surface leftovers
as "review these" instead of auto-editing — LSP precision + grep recall, with
the LLM/human adjudicating the tail.

## Packaging: out-of-process CLI

**A standalone CLI, shelled out to by the harness. [leaning]** (MCP deferred.)
Tilth is adding out-of-process tools and prefers a simple CLI over MCP. This
fits Tilth's existing seams with near-zero new infrastructure:

- **Out-of-process:** a standalone `lsp-tool` CLI (`rename`, `diagnostics`,
  `references` subcommands; JSON out). This is `lsp_raw_client.py` + the apply
  code, grown up.
- **In-process:** a thin `rename_symbol` tool whose `fn(args, workspace)`
  `subprocess.run`s the CLI — the same pattern Tilth's `bash`/`validators`
  already use. Schema + validation stay in-process (good for a weak model);
  execution is out-of-process.
- **Diagnostics via the `post_edit` hook:** generalize Tilth's ruff hook to
  shell out to `lsp-tool diagnostics <file>` for any language. Zero tool-budget
  cost — hooks aren't tools.
  - **Don't drop the linter.** A type-checker LSP (ty/gopls/rust-analyzer)
    delivers *correctness* diagnostics; a linter (ruff/clippy/eslint) delivers
    *style/idiom/bug-pattern* diagnostics. Different missions — verified on this
    repo: ty flags `sample.py`'s `greet(123)` type error while `ruff check`
    reports "All checks passed!". The hook is an *aggregator* of both. Go/Rust
    let one server cover both (gopls+staticcheck, rust-analyzer+clippy); Python
    and TS need the linter alongside (ty + ruff, tsserver + eslint).

**Why not MCP (yet):** MCP is essentially "a persistent daemon + a standardized
transport." For one harness talking to one backend, hosting an MCP server plus
writing an MCP client is more plumbing than a CLI (or a self-daemonizing CLI).
MCP earns its weight when persistence must be shared across many tools/clients.

## Stateless vs. stateful CLI — the central open question [open]

The sharp edge: a fresh-process-per-call CLI is the *one-shot model* from the
testbed, and an LSP server is the most stateful, expensive-to-start thing you
could put behind it.

| | Stateless CLI (boot per call) | Stateful (persistent / daemon) |
|---|---|---|
| Simplicity | simple — just `subprocess.run` | socket + daemon lifecycle + cleanup |
| Buffer sync | **free** — born fresh, `didOpen`s disk-truth, dies; no `didChange`, no staleness | must keep the server's buffer in sync with disk |
| Cost | **re-pays indexing every call** | index once, reuse warm |
| Sweet spot | ty/pyright/tsserver on small repos (**Tilth's stated target**) | rust-analyzer/gopls on real-sized repos |

**Leaning: start stateless.** It matches Tilth's small-project scale, and
statelessness *eliminates* the buffer-sync problem (always disk-truth by
construction) rather than solving it.

**Trigger to go stateful:** measure cold-start + index time per language on a
representative target. If Rust/Go calls are dominated by re-indexing, add
persistence **without changing the CLI interface** — a self-daemonizing mode
(first call starts a per-worktree background server on a unix socket; later
calls are thin clients; the daemon dies at session/worktree teardown, which
Tilth already owns). Precedent: **gopls `-remote=auto`**.

## Multi-language backend (`LspManager`)

A standalone manager (the grown-up `lsp_raw_client.py`), independent of the tool
layer:

- **Server registry:** extension → server command (`.py`→ty/pyright, `.ts`→
  typescript-language-server, `.rs`→rust-analyzer, `.go`→gopls).
- **Lifecycle:** stateless = born/dies per call; stateful = lazy per
  `(language, worktree)`, kept warm for the session.

Per-language reality to design for — the part that bites:

- **positionEncoding is per-server.** Read each server's *negotiated* value;
  don't hardcode utf-16 ([§5](./documentation.md#5-positions-and-the-utf-16-gotcha)).
- **Init/config differs.** rust-analyzer/gopls may require the
  `workspace/configuration` *pull* request answered — the one our testbed
  declined.
- **Indexing readiness — the real correctness risk.** A server can answer a
  `rename` *before* indexing finishes and return an *incomplete* edit set
  (silent corruption, worse than slow). The CLI must wait on a readiness signal
  (`$/progress` end, or first diagnostics) per language before issuing the op.
  The testbed doesn't do this yet.
- **Pull vs push diagnostics.** Prefer pull (`textDocument/diagnostic`,
  deterministic) where supported; otherwise take the latest pushed set after a
  settle delay.
- **`WorkspaceEdit` shape.** Handle both `changes` and `documentChanges`; the
  latter carries versioned files and `Create`/`Rename`/`Delete` resource ops
  (e.g. rust-analyzer renaming a module renames a file). The testbed applier
  punts on resource ops — the real one can't.

## Build vs. reuse [open]

Before hand-rolling five servers' quirks, evaluate **multilspy** / **solidlsp**
(the library under Serena) as the backend — both are Python and already abstract
the lifecycle/capability mess across several target languages. Keep the thin
semantic verbs on top. Hand-roll only if their coverage or edit-application
doesn't fit Tilth's worktree/disk model. `lsp_raw_client.py` stays valuable for
understanding and debugging whatever sits underneath.

## Tool-surface discipline [decided]

Tilth's rule: *"tool descriptions are prompt text; every character ships every
turn."* Ship **`rename_symbol` + the diagnostics hook** first. Add
`find_references`/`definition` as tools only when real runs show the agent
reaching for them. Don't port all of LSP.

## Correctness traps carried from the testbed

1. **UTF-16 offsets** — convert per the server's negotiated encoding
   ([§5](./documentation.md#5-positions-and-the-utf-16-gotcha)).
2. **Apply bottom-to-top** — descending by start position so edits don't shift
   later offsets ([§7](./documentation.md#7-the-rename-workflow-the-headline)).
3. **Indexing readiness** — *new for the real tool*: wait for the server to be
   ready before trusting a rename or diagnostics.

## Next step [open]

Build the stateless `lsp-tool` CLI — `rename` + `diagnostics` with the
readiness-wait and JSON output, starting with ty and structured so a second
language (gopls) drops in. Use it to measure the cold-start trade and resolve
the stateless/stateful question with evidence rather than guesswork.
