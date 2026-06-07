"""lsp-tool (Python v0) — an early trial, built on multilspy. SUPERSEDED by the
Rust implementation in ../lsp-tool-rs/; kept for ../research.md.

Same CLI contract as the Rust binary:
    uv run python lsp-tool-py/lsp_tool.py diagnostics <file>
    uv run python lsp-tool-py/lsp_tool.py rename <file> <line> <character> <new-name> [--apply]

The point of this file is the *comparison*, so it's deliberately honest about
where the library helps and where it doesn't:

  * multilspy owns the lifecycle (spawn, initialize/initialized, open_file) and
    the navigation methods (definition/references/hover/symbols) — nice.
  * It exposes **no rename and no diagnostics**, which are exactly our two
    subcommands, so both drop through to the low-level handler
    (`ls.server.send_request` / `on_notification`).
  * For Python, multilspy drives **jedi-language-server** (no server choice),
    which does navigation but not type checking — so `diagnostics` here will NOT
    see the `greet(123)` type error that the Rust v0's ty reports.

The WorkspaceEdit applier is reused from the testbed (`lsp_raw_client`).
"""

from __future__ import annotations

import argparse
import asyncio
import json
from pathlib import Path

from multilspy import LanguageServer
from multilspy.multilspy_config import MultilspyConfig
from multilspy.multilspy_logger import MultilspyLogger

# This v0 lives in a subfolder but reuses the tested WorkspaceEdit applier from
# the repo-root testbed. Put the repo root on sys.path so the import resolves
# no matter where the script is launched from.
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lsp_raw_client import apply_workspace_edit


async def _run(
    command: str,
    workspace: Path,
    rel: str,
    line: int | None = None,
    character: int | None = None,
    new_name: str | None = None,
    apply: bool = False,
) -> dict:
    config = MultilspyConfig.from_dict({"code_language": "python"})
    ls = LanguageServer.create(config, MultilspyLogger(), str(workspace))

    # multilspy has no diagnostics API — capture the pushed notifications
    # ourselves through the low-level handler.
    diagnostics_by_uri: dict[str, list] = {}
    ls.server.on_notification(
        "textDocument/publishDiagnostics",
        lambda params: diagnostics_by_uri.__setitem__(
            params["uri"], params.get("diagnostics", [])
        ),
    )

    uri = (workspace / rel).resolve().as_uri()

    async with ls.start_server():
        with ls.open_file(rel):
            if command == "diagnostics":
                # jedi pushes on didOpen; poll briefly for the notification.
                for _ in range(40):  # ~8s ceiling
                    if uri in diagnostics_by_uri:
                        break
                    await asyncio.sleep(0.2)
                return {
                    "file": rel,
                    "server": "jedi-language-server",
                    "received_push": uri in diagnostics_by_uri,
                    "diagnostics": diagnostics_by_uri.get(uri, []),
                }

            # rename: no multilspy method — raw request via the handler.
            edit = await ls.server.send_request(
                "textDocument/rename",
                {
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": character},
                    "newName": new_name,
                },
            )
            if apply and edit:
                changed = apply_workspace_edit(edit)
                return {
                    "applied": True,
                    "files_changed": sorted(str(p) for p in changed),
                    "edit": edit,
                }
            return {"applied": False, "edit": edit}


def main() -> None:
    p = argparse.ArgumentParser(description="multilspy-backed LSP CLI (v0)")
    p.add_argument("--workspace", default=".")
    sub = p.add_subparsers(dest="command", required=True)

    d = sub.add_parser("diagnostics")
    d.add_argument("file")

    r = sub.add_parser("rename")
    r.add_argument("file")
    r.add_argument("line", type=int)
    r.add_argument("character", type=int)
    r.add_argument("new_name")
    r.add_argument("--apply", action="store_true")

    args = p.parse_args()
    workspace = Path(args.workspace).resolve()
    result = asyncio.run(
        _run(
            args.command,
            workspace,
            args.file,
            line=getattr(args, "line", None),
            character=getattr(args, "character", None),
            new_name=getattr(args, "new_name", None),
            apply=getattr(args, "apply", False),
        )
    )
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
