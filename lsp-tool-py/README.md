# lsp-tool-py — early Python trial (superseded)

An early prototype of the `lsp-tool` CLI built on
[multilspy](https://github.com/microsoft/multilspy) (which drives
`jedi-language-server`). **This is not the path forward** — the tool is
implemented in Rust under [`../lsp-tool-rs/`](../lsp-tool-rs/).

It's kept only as the losing side of [`../research.md`](../research.md), which
recorded why: multilspy abstracts the lifecycle but covers neither
`rename` nor `diagnostics` (both drop to raw LSP anyway), and it locks Python to
jedi (no type-checking), while the Rust client picks the server (ty) and ships
one fast binary.

Run (from the repo root):

```bash
uv run python lsp-tool-py/lsp_tool.py diagnostics sample.py
```
