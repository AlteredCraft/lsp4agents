# Planning — lsp-tool, what's next

The forward-looking view, kept thin. The settled architecture is in
[documentation.md](./documentation.md); the rationale and explorations behind it
are in [research.md](./research.md). This file is just *current reality* + *next*.

## Current reality

A working v0 of `lsp-tool` exists in [`lsp-tool-rs/`](./lsp-tool-rs/):

- **Rust CLI**, shelled out to by the harness — `rename` + `diagnostics`, JSON out.
- **Stateless** (born → handshake → one op → die); debug perf metrics still to add.
- **One language live: Python via ty**, spawned directly from `.venv/bin/ty`.
- Reuses the testbed's UTF-16-aware, bottom-to-top `WorkspaceEdit` applier.

Settled decisions (architecture in [documentation.md](./documentation.md), the
why in [research.md](./research.md)): semantic verbs over a raw proxy;
LLM-decides-*what* / LSP-decides-*where* / tool-applies; single-repo scope; Rust,
hand-rolled; CLI over MCP; stateless-first; bring-your-own servers.

## Next steps [open]

1. **Capability negotiation.** The v0 hardcodes ty's shape (pull diagnostics,
   `prepareRename`, utf-16, `languageId: python`); pointing `--server-cmd` at jedi
   already broke on `Method Not Found: textDocument/diagnostic`. Branch on the
   server's advertised capabilities instead. (See [research.md](./research.md) §
   "spawn-decoupled, but protocol-coupled".)
2. **Second language: gopls.** The first real test of the multi-language backend,
   the per-language acquire path, and the indexing readiness-wait.
3. **Debug perf instrumentation.** Per-call timing breakdown (spawn → initialize
   → index-ready → op → total) behind a debug flag, kept out of the JSON result —
   so the eventual stateless→stateful trigger is data-driven, not a guess.
