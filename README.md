# lsp4a

Precise, semantic code operations — `rename`, `references`, and `diagnostics` —
for LLM coding agents, backed by real Language Server Protocol servers instead
of `grep`-and-replace.

**The premise: LSP is editor-shaped; an agent is intent-shaped.** An editor has a
cursor, so a code position is free. An agent has only a symbol name and an
intent. The hard part — turning "rename `getUser`" into precise zero-indexed
UTF-16 coordinates and a verified, cross-file edit — is exactly what this tool
does, so the model never has to count columns (and quietly corrupt a string or
miss a reference). You say `lsp4a rename sample.py greet salutation`; the tool
finds the symbol, verifies it with the server, applies the edit, and answers in
JSON — a structured before/after summary, or an "ambiguous, here are the
candidates" error when two scopes share the name.

> **Status: early, active research.** This is a research repo first, not a
> product. A working v0 — `lsp4a`, a stateless Rust CLI — does `rename` +
> `references` + `diagnostics` against [ty](https://docs.astral.sh/ty/) for
> Python; Go, Rust, and TypeScript are the next targets. Usable by any agent
> harness that can shell out — [Tilth](https://github.com/AlteredCraft/tilth)
> is a candidate future integration, not a driver: nothing here is sequenced
> against its schedule.

## How we got here

It began as a raw LSP exploration — a from-scratch client speaking JSON-RPC over
stdio, to learn the protocol on the wire rather than through a library. That
surfaced the design (semantic verbs over a transparent proxy; the UTF-16 and
apply-order traps) and a build-vs-reuse question, settled in Rust's favor by a
Rust-vs-Python bake-off. Three docs carry the thinking:

- **[documentation.md](./documentation.md)** — the decided architecture plus an
  LSP protocol reference (diagrams, glossary, spec links).
- **[research.md](./research.md)** — the rationale, and the Rust-vs-Python
  comparison (behavior, lines-of-code-to-maintain, latency).
- **[planning.md](./planning.md)** — thin and forward-looking: what's next.

## Setup

`lsp4a` brings its own language server (BYO model). For Python that's
[ty](https://docs.astral.sh/ty/), a standalone binary — **no Python runtime**:

```bash
# 1. install ty onto your PATH (pin a version for reproducibility)
curl -LsSf https://astral.sh/ty/install.sh | sh

# 2. install the lsp4a CLI onto your PATH (release build → ~/.cargo/bin)
cargo install --path lsp4a
```

ty is the only runtime dependency; if it isn't found, `lsp4a` fails fast with
that same install hint.

## The tool: `lsp4a`

A stateless Rust CLI ([`lsp4a/`](./lsp4a/)) the harness shells out to
for `rename`, `references`, and `diagnostics`, JSON on stdout (errors too:
`{"error": {...}}`, exit 1). It spawns a language server as a subprocess and
speaks LSP over stdio — the language-agnostic seam (ty is Rust, gopls is Go; the
client doesn't care). It finds `ty` on your PATH by default; point
`--server-cmd` at any other server.

Run from your project root (`--workspace` defaults to `.`):

```bash
lsp4a rename sample.py greet salutation
lsp4a references sample.py greet
lsp4a diagnostics sample.py
```

Hacking on lsp4a itself? Run from source without installing:
`cargo run --manifest-path lsp4a/Cargo.toml -- rename sample.py greet salutation`.

`rename` and `references` take a **target**: a symbol name (`greet`), or an
explicit `line:character` position (zero-indexed, UTF-16 column — `5:10`) as the
escape hatch when a name is ambiguous; the ambiguity error lists every candidate
with its line text so the caller can pick one. How resolution works (lexical
scan → `prepareRename` verify → `references` dedupe) is in
[research.md](./research.md) § "the v0 interface leaked positions".

`rename` returns a **structured summary** an agent reads directly — `status`
(`preview`/`applied`), `scope` (`{files, edits}`), and a before/after row per
changed line — never a raw `WorkspaceEdit`. Add `--apply` to edit the files in
place (restore with `git checkout sample.py consumer.py`), or `--raw` to also
get the underlying `WorkspaceEdit` for callers that apply it themselves.

## FAQ

**Is a bare symbol name specific enough? What if a function and a variable
share the name `greet`?**
Then the tool refuses to guess: exit 1 with a structured error whose
`error.data.candidates` lists every verified occurrence with its line number
and source text, plus a hint to re-run with `line:char`. The design property
is that the failure mode is never "renamed the wrong thing" — the
reference-set check can't merge two distinct symbols (a position inside a
reference range of symbol A *is* a reference to A), so an imperfect server can
only cause a *false ambiguity*, which fails safe. For an agent the loop is:
try the name; on ambiguity, read the candidates (judgment — its strength) and
re-run with coordinates the tool supplied, not ones it counted.

**Does rename work across files? Do I have to point at the definition?**
Workspace-wide, and no. The `<file>` argument only anchors *which symbol you
mean*; the server indexes the whole `--workspace` and resolves the import
graph, so once the symbol is pinned, `rename`/`references` cover every file in
the repo. Naming a *caller's* file is identical to naming the definition's —
one rename edits the `def`, the `from lib import greet` lines, and
attribute-style `lib.greet(...)` calls alike.

**What can't it see?**
Anything not statically resolvable in the workspace: dynamic references
(`getattr`, string keys) and consumers in *other* repos
([documentation.md §8](./documentation.md#8-why-lsp-rename-beats-grep--and-its-blind-spot)).
Planned mitigation: after a rename, grep the old name and surface the residue
as "review these" ([planning.md](./planning.md)).

**Why not just grep?**
Grep over-matches (strings, comments, unrelated same-named symbols); LSP
under-matches (the dynamic refs above). Rename wants precision, so LSP leads
and grep is the planned recall backstop — the precision/recall table is in
[documentation.md §8](./documentation.md#8-why-lsp-rename-beats-grep--and-its-blind-spot).

**Why a CLI instead of an MCP server, like Serena?**
For one harness talking to one backend, a CLI is less plumbing and more
agent-legible; the prior-art comparison (Serena, cclsp, mcp-language-server)
and the honest costs are in [research.md](./research.md) § "A CLI, not MCP
(yet)".

**Doesn't `ty check` already do diagnostics?**
Yes — batch CLIs cover the stateless case, which is why diagnostics is the
*weak* half of the pitch ([research.md](./research.md) § "Which verbs actually
pay"). It stays because it's one interface and nearly free; `rename` and
`references` are the verbs nothing in an agent's stock toolkit replaces.

**Doesn't booting a fresh server every call get slow?**
Server cold-start dominates and ty's is ~0.1s, fine for small-to-mid repos
(measurements in [research.md](./research.md)). For gopls/rust-analyzer-class
servers on real repos the planned self-daemonizing mode — same CLI interface —
is the answer ([planning.md](./planning.md)). Correctness never degrades with
repo size; only latency does.

## Files

- `documentation.md` — the decided architecture of `lsp4a` plus an LSP protocol
  reference (message shapes, lifecycle, rename workflow, diagnostics) with
  diagrams, a glossary, and spec links.
- `research.md` — the rationale behind the design, and the Rust-vs-Python v0
  comparison (behavior, lines-of-code-to-maintain, latency).
- `planning.md` — thin and forward-looking: current reality and next steps.
- `lsp4a/` — **the tool**, in Rust: hand-rolled framing, symbol→position
  resolution, a ty-driven `rename`/`references`/`diagnostics` CLI, and a
  UTF-16-aware applier. Build/run with `cargo`; `cargo test` covers the
  resolution scanner, URI handling, and an end-to-end `rename` suite
  (`tests/rename.rs`) that drives the built binary against fixture workspaces.
- `sample.py` — small program defining `greet`, with a deliberate type
  error (`greet(123)` where `greet` expects `str`) plus two decoy uses of
  the word "greet" (a comment and an f-string) that rename must ignore.
- `consumer.py` — imports and calls `greet` from `sample.py`, so a single
  rename produces edits in two files (the cross-file case).

## References

[documentation.md](./documentation.md) carries the concept notes and the deep
links into the LSP 3.17 spec. External:
[JSON-RPC 2.0](https://www.jsonrpc.org/specification) ·
[ty docs](https://docs.astral.sh/ty/).
