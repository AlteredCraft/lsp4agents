# lsp4a — project guide for Claude

`lsp4a` is a stateless Rust CLI that gives LLM agents precise, semantic LSP
operations — `rename`, `references`, `diagnostics` — so a model never has to
count UTF-16 columns or synthesize an edit list. The crate lives in `lsp4a/`.
Architecture is in `documentation.md`, rationale + the Rust-vs-Python bake-off in
`research.md`, current state + next steps in `planning.md`.

## This is a Rust project, not Python
- Build/test/run with `cargo`; the crate is `lsp4a/` (use `--manifest-path
  lsp4a/Cargo.toml`, or `cd lsp4a`).
- **The parent CLAUDE.md's "always use uv" rule does not apply here** — there is
  no Python toolchain (no `pyproject.toml`/uv/venv). `sample.py` and
  `consumer.py` are *test fixtures* the tool operates on, not a package to manage.
- Install the CLI: `cargo install --path lsp4a` (→ `~/.cargo/bin/lsp4a`). Re-run
  after source changes to refresh the installed binary.

## Language server (bring-your-own)
- Python is driven by **ty**, a standalone binary — no Python runtime. Default
  `--server-cmd` is `ty server` (found on PATH).
- Install: `curl -LsSf https://astral.sh/ty/install.sh | sh` (pin a version for
  reproducibility). If ty is missing, the tool fails fast with this hint.

## The output contract (do not violate)
This is the whole point of the tool — keep every code path consistent with it:
- **Structured JSON on stdout, always** — every command and every failure.
- **Agent-legible, never protocol coordinates.** Results speak in 1-indexed
  lines + source text; never expose 0-indexed / UTF-16 `character` columns in the
  default output. Raw protocol objects (`WorkspaceEdit` / `Location[]` / LSP
  diagnostics) go behind `--raw`.
- **Errors are `{"error": {"message", "data"?}}` on stdout** — exit 1 for runtime
  failures, exit 2 for usage errors (which add a `usage` field). Even clap parse
  errors are funneled here; never let prose reach the user as stderr output.
- **Server logs are noise** — suppressed unless `--debug` (a global flag).

## Testing
- Unit tests live in `src/*.rs`; the end-to-end CLI suite is `lsp4a/tests/cli.rs`,
  driving the *built* binary against fixture workspaces and a real ty.
- The integration tests **skip** (not fail) when ty isn't on PATH.
- Create fixtures **outside the repo tree** (the suite uses the system temp dir):
  ty walks up for project config, so a workspace nested under an enclosing
  project is analyzed as part of *that* project and `prepareRename` returns null.
- Run `cargo test --manifest-path lsp4a/Cargo.toml`; keep `cargo clippy` clean.
