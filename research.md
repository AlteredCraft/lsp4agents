# Research & rationale

The *why* behind `lsp-tool` — the framing, the trade-offs, and the evidence
behind each decision. The decisions themselves live as settled architecture in
[documentation.md](./documentation.md); what's next is in
[planning.md](./planning.md). This file is the archive of how we got there.

## The framing that drives everything

> **LSP is editor-shaped; an agent is intent-shaped.** LSP was designed assuming
> a human with a cursor — the position is free, it's wherever the caret is. An
> LLM has no cursor. It has a symbol name and an intention. So the single
> hardest part of using LSP, resolving "the symbol `getUser`" to a precise
> zero-indexed UTF-16 (line, character), is exactly what a transparent proxy
> refuses to do — it dumps that on the LLM's arithmetic, which is the weakest
> link. The model will miscount a column, especially with the UTF-16 thing we
> hit, and you'll get a rename of the wrong span or a no-op.

Everything below follows from that.

## Why these choices

### Semantic verbs, not a transparent proxy
The tool's whole value is being the impedance transformer between intent and
protocol coordinates — exactly what a proxy refuses to do. Two forces reinforce
it: **multi-language** (one normalized verb hides five servers' encoding/
capability/init differences) and **weak open models** (a deepseek-class model is
the most likely to botch raw position arithmetic, so never hand it positions).

### Division of labor
The LLM is good at judgment and bad at exhaustive mechanical reference-finding;
the LSP is the reverse. So the LLM decides *what* to rename and *to what*; the
LSP decides *where* every reference is; the tool *applies* the `WorkspaceEdit`.
The LLM never synthesizes an edit list.

### Single-repo scope
The LSP only sees what's statically resolvable in the workspace, so that's the
boundary. Dynamic refs (`getattr`, string keys) and cross-repo consumers are out
of scope. Mitigation for later: after a rename, `grep` the *old* name and surface
the residue as "review these" — LSP precision plus grep recall, with the
LLM/human adjudicating the tail.

### A CLI, not MCP (yet)
Tilth is adding out-of-process tools and prefers a simple CLI. MCP is essentially
"a persistent daemon + a standardized transport"; for one harness talking to one
backend, hosting an MCP server plus writing an MCP client is more plumbing than a
CLI (or a self-daemonizing CLI). MCP earns its weight when persistence must be
shared across many tools/clients.

### Stateless first
A fresh-process-per-call CLI is the most stateful-server-behind-a-stateless-shell
arrangement possible — but at Tilth's small-project scale the trade is good, and
statelessness *eliminates* buffer sync rather than solving it.

| | Stateless CLI (boot per call) | Stateful (persistent / daemon) |
|---|---|---|
| Simplicity | simple — just `subprocess.run` | socket + daemon lifecycle + cleanup |
| Buffer sync | **free** — born fresh, `didOpen`s disk-truth, dies | must keep the server's buffer in sync with disk |
| Cost | **re-pays indexing every call** | index once, reuse warm |
| Sweet spot | ty/pyright/tsserver on small repos | rust-analyzer/gopls on real-sized repos |

**Trigger to go stateful:** when the debug perf metrics show a language's calls
dominated by re-indexing (expected for rust-analyzer/gopls on real repos), add a
self-daemonizing mode *without changing the CLI interface* — first call starts a
per-worktree background server on a unix socket; later calls are thin clients;
the daemon dies at session/worktree teardown. Precedent: **gopls `-remote=auto`**.

### Bring-your-own servers
The tool doesn't bundle servers (bundling N servers × M platforms is a release
nightmare). Each is BYO on a known path, with per-language acquire instructions
and "detect missing → fail fast with an install hint." Pin a version per language
so rename/diagnostics stay reproducible. Python is decided: uv acquires + pins ty
via `uv.lock`, and the tool execs `.venv/bin/ty` directly (no `uv run` wrapper).

### Rust, hand-rolled (not Python / multilspy)
Settled by the v0 comparison below. In short: multilspy abstracts only the
lifecycle — it covers neither rename nor diagnostics, so the operations that
matter drop to raw LSP regardless — and it's Python, so a Rust tool can't reuse
it (nor solidlsp). The hand-rolled Rust client owns the lifecycle itself (a few
hundred lines) with full control over server choice, encodings, and the apply
path.

---

## The Rust-vs-Python v0 comparison

Two implementations of the same CLI contract (`rename` + `diagnostics`, JSON
out), built to put evidence behind the build-vs-reuse question:

- **`lsp-tool-rs/`** — hand-rolled Rust, drives **ty**. Framing, lifecycle, and
  the WorkspaceEdit applier are all written here. **(The chosen implementation.)**
- **`lsp-tool-py/`** — Python on **multilspy**, which drives
  **jedi-language-server** (no server choice). **(An early trial.)**

Both were run against this repo's `sample.py` / `consumer.py`.

### Behavioral findings

| | Rust (→ ty) | Python (multilspy → jedi) |
|---|---|---|
| `diagnostics sample.py` | catches `invalid-argument-type` on `greet(123)` (pull) | **nothing** — jedi doesn't type-check; no push at all |
| `rename` (cross-file) | 4 edits, **`changes`** encoding | 4 edits, **`documentChanges`** encoding |
| `--apply` | UTF-16 + bottom-to-top; decoys untouched | the *same* applier handled `documentChanges` |

- **Server lock-in cost the type error.** multilspy offers no Python-server
  choice (jedi); the hand-rolled client picked ty and caught the bug.
- **Both WorkspaceEdit encodings showed up for real** — ty `changes`, jedi
  `documentChanges` — on the same rename. "Handle both" was not theoretical.
- **multilspy is navigation-only** (no rename, no diagnostics), so both
  subcommands dropped through to its low-level handler anyway.

### Lines of code — maintenance view

cloc `code` lines (blanks/comments excluded).

| | Rust (→ ty) | Python (multilspy → jedi) |
|---|---|---|
| **Code you author & maintain** | **373** | **~165** (85 in `lsp-tool-py/lsp_tool.py` + ~80 reused applier) |
| Deps — raw source pulled in | 452,834 | 175,296 |
| &nbsp;&nbsp;– platform-dead `windows-*` (not built on macOS) | −318,762 | — |
| &nbsp;&nbsp;– build-time proc-macros (`syn` etc.; not shipped) | −74,937 | — |
| **Deps — compiled / runtime-resident** | ≈ **59,135** | **175,296** (all interpreted) |
| Ships as | one **1.2 MB** static binary | Python runtime + the 175K-line tree |

**Raw dep LOC misleads.** ~88% of Rust's 453K is `windows-sys` (`cfg`-gated,
never compiled on macOS, confirmed via `cargo tree --target`) plus proc-macro
machinery (build-time, not in the binary). Real macOS-shipped Rust deps ≈ 59K.
Compared like-for-like (core analysis + protocol), the two runtime trees are the
**same order of magnitude — ~60K each**: Python's jedi + parso + pygls +
lsprotocol + multilspy ≈ 60K of core code; the rest is incidental
`requests`/`idna`/`urllib3`/`psutil` that only load when downloading a server.

**Maintenance takeaway:** LOC favors the library on *authored* code (~165 vs
373) — multilspy ate the framing + lifecycle. But since it lacks rename/
diagnostics, you write and own glue around it regardless, and inherit its server
choice (jedi) and its churn (v0.0.15). The hand-rolled route trades ~200 extra
lines you own for server choice (ty's type-checking), a single fast binary, and
no runtime dependency on an early-stage library for the operations that matter.

### Startup / per-call latency

A naive `time` of each v0 is **not** apples-to-apples — two artifacts dominate
the headline gap:

- the Python `diagnostics` command sleeps ~8s polling for a push jedi never
  sends (`rename` on the same harness is ~1s);
- `cargo run` adds ~0.2s of build-freshness checking the shipped binary skips.

Stripped to a fair comparison (`sample.py`, warm caches):

| harness → server | operation | time |
|---|---|---|
| Rust binary → ty | rename | **0.08s** |
| Python (lean, `lsp_raw_client.py`) → ty | full session | **0.14s** |
| Python (multilspy) → jedi | rename | **1.08s** |

**Server cold-start dominates; the harness language barely matters.** Python and
Rust both driving ty land in the same ~0.1s ballpark — the v0s' real gap is
**jedi vs ty** (multilspy's server lock-in again), not Python vs Rust. The Python
startup floor (`uv run python -c pass`) is ~0.03s. This is why per-call cost is
mostly server cold-start: pick a fast-starting server, and a warm-server daemon's
payoff is amortizing exactly that — ~0.1–0.3s/call for ty, but
seconds-to-minutes/call for rust-analyzer/gopls on a real repo.

## Finding: spawn-decoupled, but protocol-coupled

The Rust client treats the server as a swappable command string — pointing
`--server-cmd` at jedi instead of ty *spawns* fine. But the v0 hardcodes ty's
protocol shape (pull diagnostics, `prepareRename`, utf-16, `languageId: python`),
so jedi immediately returned `Method Not Found: textDocument/diagnostic` (jedi
only pushes diagnostics). The process boundary is fully decoupled; the protocol
interactions are not — true multi-server support needs **capability negotiation**
(read the server's advertised capabilities from `initialize` and branch). That's
the first item in [planning.md](./planning.md)'s next steps.
