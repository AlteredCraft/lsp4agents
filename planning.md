# Planning — lsp4a, what's next

The forward-looking view, kept thin. The settled architecture is in
[documentation.md](./documentation.md); the rationale and explorations behind it
are in [research.md](./research.md). This file is just *current reality* + *next*.

## Current reality

A working v0 of `lsp4a` exists in [`lsp4a/`](./lsp4a/):

- **Rust CLI** (`lsp4a`), shelled out to by the harness — `rename`,
  `references`, `diagnostics`, JSON out (errors too: `{"error": {...}}`, exit 1).
- **Symbol targets.** `lsp4a rename sample.py greet salutation` — the tool
  resolves the name to protocol coordinates (lexical scan → `prepareRename`
  verify → `references` dedupe; see research.md § "the v0 leaked positions").
  Explicit `line:char` remains as the disambiguation escape hatch, and ambiguity
  is a structured error listing the candidates.
- **Structured rename output (not a raw WorkspaceEdit).** The result is a
  presentation an agent reads directly — `status` (`preview`/`applied`), `scope`
  (`{files, edits}`), and a before/after row per changed line — never protocol
  ranges or UTF-16 columns. The raw `WorkspaceEdit` is available behind `--raw`
  for callers that apply it themselves. This is the output-side counterpart to
  the input-side symbol-resolution layer: the same impedance transformer that
  research.md § "the v0 interface leaked positions" warned must not leak.
- **Uniform agent-legible output across verbs.** `references` and `diagnostics`
  speak the same language as `rename` — 1-indexed lines + source text, never
  UTF-16 columns or 0-indexed coordinates; `diagnostics` maps severity to a word
  and carries related-location context. Each verb takes `--raw` to also emit the
  underlying protocol object. The server's own logs are suppressed unless
  `--debug` (a global flag), so the JSON is the only thing on the output. Even
  clap usage errors (missing/bad args) come back as the same `{"error": ...}`
  envelope on stdout (exit 2, with a `usage` field), not prose on stderr.
- **Stateless** (born → handshake → one op → die), with a per-response
  `--timeout` so a wedged server fails fast; debug perf metrics still to add.
- **One language live: Python via ty**, run as `ty server` from PATH (ty is a
  standalone binary, no Python — install with Astral's pinned installer);
  `languageId` is derived from the file extension, not hardcoded.
- Reuses the testbed's UTF-16-aware, bottom-to-top `WorkspaceEdit` applier.

Settled decisions (architecture in [documentation.md](./documentation.md), the
why in [research.md](./research.md)): semantic verbs over a raw proxy — now
including the symbol-resolution layer that makes them real;
LLM-decides-*what* / LSP-decides-*where* / tool-applies; single-repo scope; Rust,
hand-rolled; CLI over MCP; stateless-first; bring-your-own servers.

**Framing:** this is a research project. Tilth is a candidate future consumer,
not a driver — nothing below is sequenced against its (or any integration's)
schedule.

## Next steps [open]

Ordered by agent value, not protocol completeness:

1. **Capability negotiation.** The v0 still hardcodes most of ty's shape and
   assumes ty's answers hold for any server. Branch on the server's advertised
   `initialize` capabilities instead. Started: `positionEncoding` is guarded
   (a non-utf-16 server fails fast rather than corrupting edits),
   `referencesProvider` gates `references`, and `diagnostics` now gates the pull
   request on `diagnosticProvider` and degrades to pushed diagnostics otherwise
   — so a push-only server (the jedi `Method Not Found:
   textDocument/diagnostic` break) no longer errors. Still unbranched:
   `rename`/`prepareRename` fire unconditionally, and `prepare_rename` swallows
   *all* errors as "not renameable," conflating "server lacks rename support"
   with "this position isn't renameable" — gate on `renameProvider` and
   distinguish the two.
   Sequencing: this is the enabling half of "add a second server" (#2), not a
   solo sprint. ty advertises everything the v0 assumes, so with only ty on the
   bench the remaining branches can't be exercised — land them together with
   gopls, driven by its concrete capability set rather than speculation off one
   jedi error. (See [research.md](./research.md) §
   "spawn-decoupled, but protocol-coupled".)
2. **Second language: gopls.** The first real test of the multi-language
   backend, the per-language acquire path, and the indexing readiness-wait.
   Note: for gopls/rust-analyzer-class servers the warm daemon (below) is
   closer to a prerequisite than an optimization — see research.md § "where
   the stateless sweet spot ends".
3. **Debug perf instrumentation.** Per-call timing breakdown (spawn → initialize
   → index-ready → op → total) behind a debug flag, kept out of the JSON result —
   so the stateless→stateful trigger is data-driven, not a guess.
4. **Self-daemonizing mode** for slow-indexing servers, same CLI interface
   (gopls `-remote=auto` precedent). Sequenced after (3) so it lands with
   evidence, but expected to be required for gopls/rust-analyzer on real repos.
5. **Hybrid rename residue.** After `--apply`, grep the *old* name and surface
   leftovers as "review these" — LSP precision plus grep recall.

## Engineering needs [open]

Hygiene the research depends on, kept separate from the agent-value ordering
above:

- **End-to-end integration tests — landed for all three verbs.** Unit tests
  cover the pure parts (lexical scanner, URI handling, the WorkspaceEdit
  applier); [`lsp4a/tests/cli.rs`](./lsp4a/tests/cli.rs) runs the *built* binary
  against fixture workspaces and asserts the JSON contract: the structured rename
  preview, decoy comment/string filtering, `--raw`, cross-file rename with
  `--apply` (and decoys untouched on disk), the shadowing → structured-ambiguity
  error, the unknown-symbol error, the `line:char` escape hatch, and that
  `references`/`diagnostics` return 1-indexed lines + source text (never UTF-16
  columns) with the protocol forms behind `--raw`. This is the safety net the
  invasive protocol work below (capability negotiation, a second server) leans
  on. Remaining gap: the timeout / not-renameable error paths are asserted only
  via the ambiguity and unknown-symbol cases — worth their own fixtures before
  the protocol churn.
  - **Fixture gotcha worth keeping:** the fixture workspace must live *outside*
    any enclosing project (the suite uses the system temp dir). ty walks up
    looking for project config, so a workspace nested under an outer project's
    config (e.g. a `pyproject.toml`) is analyzed as part of *that* project and
    the fixture file reads as out-of-project — `prepareRename` returns null for
    every candidate. A real consumer whose target repo is itself nested in a
    larger project could hit the same edge; flagged here, not yet handled in
    the tool.
