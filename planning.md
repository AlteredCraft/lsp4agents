# Planning — lsp-tool, what's next

The forward-looking view, kept thin. The settled architecture is in
[documentation.md](./documentation.md); the rationale and explorations behind it
are in [research.md](./research.md). This file is just *current reality* + *next*.

## Current reality

A working v0 of `lsp-tool` exists in [`lsp-tool-rs/`](./lsp-tool-rs/):

- **Rust CLI**, shelled out to by the harness — `rename`, `references`,
  `diagnostics`, JSON out (errors too: `{"error": {...}}`, exit 1).
- **Symbol targets.** `rename sample.py greet salutation` — the tool resolves
  the name to protocol coordinates (lexical scan → `prepareRename` verify →
  `references` dedupe; see research.md § "the v0 leaked positions"). Explicit
  `line:char` remains as the disambiguation escape hatch, and ambiguity is a
  structured error listing the candidates.
- **Stateless** (born → handshake → one op → die), with a per-response
  `--timeout` so a wedged server fails fast; debug perf metrics still to add.
- **One language live: Python via ty**, spawned directly from `.venv/bin/ty`;
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

1. **Capability negotiation.** The v0 still hardcodes most of ty's shape (pull
   diagnostics with push fallback, `prepareRename`); pointing `--server-cmd` at
   jedi broke on `Method Not Found: textDocument/diagnostic`. Branch on the
   server's advertised capabilities (started: `referencesProvider` and
   `positionEncoding` are checked; the rest isn't). (See
   [research.md](./research.md) § "spawn-decoupled, but protocol-coupled".)
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

- **End-to-end integration tests — a documented need, deliberately not yet
  implemented.** Unit tests cover the pure parts (lexical scanner, URI
  handling, the WorkspaceEdit applier); the full rename-through-ty path was
  verified manually ([research.md](./research.md)). The gap: a suite that runs
  the built binary against fixture workspaces and asserts on the JSON
  contract — decoy comment/string filtering, cross-file rename with `--apply`,
  the shadowing → structured-ambiguity error, unknown-symbol /
  not-renameable / timeout errors, and the `line:char` escape hatch. It
  matters most as a safety net *before* the invasive protocol work above
  (capability negotiation, a second server); until it exists, protocol
  refactors re-pay manual verification.
