# Rust vs. Python v0 — comparison

Two implementations of the same CLI contract (`rename` + `diagnostics`, JSON
out), built to put evidence behind the build-vs-reuse question in
[planning.md](./planning.md):

- **`lsp-tool-rs/`** — hand-rolled Rust, drives **ty**. Framing, lifecycle, and
  the WorkspaceEdit applier are all written here.
- **`lsp_tool_mls.py`** — Python on **multilspy**, which drives
  **jedi-language-server** (no server choice).

Both were run against this repo's `sample.py` / `consumer.py`.

## Behavioral findings

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

## Lines of code — maintenance view

cloc `code` lines (blanks/comments excluded).

| | Rust (→ ty) | Python (multilspy → jedi) |
|---|---|---|
| **Code you author & maintain** | **373** | **~165** (85 in `lsp_tool_mls.py` + ~80 reused applier) |
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

**Takeaway (maintenance):**
- *My code:* the library route is smaller (~165 vs 373) — multilspy ate the
  framing + lifecycle. But since it lacks rename/diagnostics, you write and own
  glue around it regardless, and inherit its server choice (jedi) and its churn
  (it's at v0.0.15).
- *Their code:* Rust pulls far more raw lines, but they're bedrock
  (`serde`/`clap`) or dead/build-time — code you'll essentially never debug.
  The Python tree is smaller yet leans on a young, navigation-only library you
  are already working around.

Net: LOC favors the library on *authored* code, but the hand-rolled route trades
~200 extra lines you own for server choice (ty's type-checking), a single fast
binary, and no runtime dependency on an early-stage library for the operations
that actually matter.
