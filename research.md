# Research & rationale

The *why* behind `lsp4a` — the framing, the trade-offs, and the evidence
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
The consumer shape this research targets is an agent harness shelling out to a
subprocess; Tilth — a candidate future integration, not a schedule driver — is
the concrete example, and it prefers a simple CLI. MCP is essentially
"a persistent daemon + a standardized transport"; for one harness talking to one
backend, hosting an MCP server plus writing an MCP client is more plumbing than a
CLI (or a self-daemonizing CLI). MCP earns its weight when persistence must be
shared across many tools/clients. A CLI is also the more agent-legible shape:
models are heavily trained on shelling out, and JSON-on-stdout composes with
any harness that has a bash tool.

**Prior art (the "why not Serena?" question).** This space has incumbents, and
they're MCP-shaped: **Serena** (MCP server over solidlsp, the most adopted),
**cclsp**, **mcp-language-server**. They validate the premise — agents plus
LSP is a real category — and they chose MCP partly because a persistent server
solves the warm-index problem for free. Why build anyway: (a) the target shape
is a plain CLI, and none of the incumbents offer one; (b) they inherit the multilspy/
solidlsp lineage's server lock-in, which the bake-off below showed costs real
results (jedi missing the type error ty catches); (c) a single static binary
with no Python runtime is a different deployment point. The honest flip side:
when this project reaches the daemon stage, it will have rebuilt a chunk of
what MCP gives for free — that's the price of the CLI interface, paid
knowingly. Revisit if a real consumer (Tilth or otherwise) lands on MCP.

### Stateless first
A fresh-process-per-call CLI is the most stateful-server-behind-a-stateless-shell
arrangement possible — but at the small-repo scale this research targets first,
the trade is good, and statelessness *eliminates* buffer sync rather than
solving it.

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

**Where the stateless sweet spot ends — an honest mismatch.** Frontier models
with grep + edit are *good enough* on small repos, which is exactly where
stateless LSP is cheap. Where agents actually fall apart — large repos,
cross-file refactors — is where stateless is expensive (rust-analyzer/gopls
cold-start is seconds to minutes per call). So the stateless sweet spot and the
*value* sweet spot don't fully overlap, and for big-repo languages the daemon
is closer to a prerequisite than an escalation. The stateless v0 is still the
right first move (it proves the verbs and the apply path with zero lifecycle
machinery), but the daemon is on the critical path to the tool mattering, not
a contingency.

### Which verbs actually pay (diagnostics is the weak half)
Calibrating the verbs against what agents can't already do:

- **`rename` is the headline.** Wide mechanical renames are among the most
  reliable ways agents corrupt code — miss references, edit look-alikes,
  lose patience on file 7 of 12. One deterministic call replaces an
  error-prone N-step edit loop. Nothing in an agent's stock toolkit
  substitutes.
- **`references` is arguably worth more than rename** and cheaper to serve:
  it's both impact analysis *before* an edit and a precise, token-cheap
  answer to "who calls this?" that grep fan-out gets wrong (same-named
  symbols) and pays for in context.
- **`diagnostics` is the soft spot.** Most type checkers ship batch CLIs —
  `ty check`, `tsc --noEmit`, `gopls check`, `rust-analyzer diagnostics` — so
  an agent (or a post-edit hook) gets the same answer with `subprocess.run`
  and zero protocol plumbing. In the stateless v0, `lsp4a diagnostics` is
  a more elaborate `ty check`. The LSP route earns its keep only once a warm
  daemon makes it *incremental* — another reason the daemon is on the
  critical path. The verb stays (one interface, and it's nearly free once
  the client exists), but it's not the pitch.

### Bring-your-own servers
The tool doesn't bundle servers (bundling N servers × M platforms is a release
nightmare). Each is BYO on a known path, with per-language acquire instructions
and "detect missing → fail fast with an install hint." Pin a version per language
so rename/diagnostics stay reproducible. Python is decided: **ty is a standalone
binary (no Python runtime)** installed onto PATH via Astral's version-pinned
installer (`curl -LsSf https://astral.sh/ty/<version>/install.sh | sh`); the tool
runs `ty server` from PATH. (An earlier iteration acquired ty as a PyPI
dependency through a uv-managed `.venv` — needless Python coupling for what is
already a native binary; dropped.)

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

- **`lsp4a/`** — hand-rolled Rust, drives **ty**. Framing, lifecycle, and
  the WorkspaceEdit applier are all written here. **(The chosen implementation.)**
- **`lsp-tool-py/`** — Python on **multilspy**, which drives
  **jedi-language-server** (no server choice). **(An early trial, since removed
  now that Rust is settled — kept in git history; the findings below are the
  evidence it left behind.)**

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

## Finding: the v0 interface leaked positions (fixed)

The first v0 betrayed its own thesis. The docs said "the LLM never sees a
`{line, character}`" — but the CLI was `rename <file> <line> <character>
<new-name>`, zero-indexed UTF-16 columns and all. It handed the model exactly
the arithmetic the framing calls the weakest link; the impedance transformer
was a pass-through. Worth recording because it's an easy failure mode: the
protocol's shape quietly becomes the tool's shape, one plumbing layer at a
time.

The fix is the **symbol-resolution layer**: the target argument is now a
symbol name, resolved in three stages, each leaning on the server for the
semantic judgment so the lexical part only *proposes*:

1. **Lexical scan** — word-boundary occurrences of the identifier in the file,
   with UTF-16 columns computed by the tool (over-matches strings/comments by
   design);
2. **`prepareRename` verification** — each candidate is asked "renameable
   here?"; strings, comments, and keywords return null and drop out (the
   sample.py decoys die here);
3. **`references` dedupe** — if several candidates survive, occurrences of the
   *same* symbol are exactly its reference set, so pull references from the
   first and check the rest are covered. Anything uncovered is a genuinely
   different symbol (shadowing) → a structured `error.data` listing every
   candidate with its line text, and the `line:char` escape hatch (which the
   CLI still accepts as a target — `5:10` can't be an identifier) for the
   caller to disambiguate.

Verified against ty: `rename sample.py greet salutation` resolves through the
comment/string decoys to 4 edits in 2 files; two same-named locals in
different scopes correctly come back ambiguous; `rename shadow.py 1:4 total`
then renames just the one scope. An alternative considered: `documentSymbol`/
`workspace/symbol` lookup — rejected for v0 because symbol-listing coverage
varies across servers, while `prepareRename` + `references` are the same
methods the verbs already need.
