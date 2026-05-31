"""Minimal raw LSP client for poking at ty's language server.

The goal is visibility, not abstraction: every JSON-RPC frame we send or
receive is dumped to stdout with the Content-Length header preserved, so
the protocol stays on the page. See README.md for a full walkthrough.

Run with: `uv run python lsp_raw_client.py`
"""

from __future__ import annotations

import json
import subprocess
import sys
import threading
import time
from pathlib import Path
from urllib.parse import unquote, urlparse


PROJECT_ROOT = Path(__file__).parent.resolve()
SAMPLE_FILE = PROJECT_ROOT / "sample.py"
CONSUMER_FILE = PROJECT_ROOT / "consumer.py"  # imports greet from sample.py


# ─── Framing ────────────────────────────────────────────────────────────────
# LSP runs JSON-RPC 2.0 over a stream (here, stdio). It is NOT line-delimited:
# each message is prefixed by HTTP-style headers, the only required one being
# `Content-Length: N`. Headers end with a blank line (\r\n\r\n), then exactly
# N bytes of UTF-8 JSON follow. Read exactly N — do NOT readline the body, the
# JSON can contain newlines.

class Framer:
    """Reads/writes LSP JSON-RPC frames over a subprocess's stdio."""

    def __init__(self, proc: subprocess.Popen[bytes]) -> None:
        self.proc = proc
        self._next_id = 0

    def new_id(self) -> int:
        # JSON-RPC requests carry an `id` so responses can be correlated.
        # Notifications (one-way messages) omit `id`. Monotonic ints are fine.
        self._next_id += 1
        return self._next_id

    def send(self, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        # The spec is strict: Content-Length counts *bytes*, not characters,
        # and the header block ends with CRLF CRLF.
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        _dump("→ SEND", header, payload)
        assert self.proc.stdin is not None
        self.proc.stdin.write(header + body)
        self.proc.stdin.flush()

    def recv(self) -> dict:
        assert self.proc.stdout is not None
        # 1) Read headers line-by-line until the blank separator.
        headers: dict[str, str] = {}
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server closed stdout before sending a frame")
            if line in (b"\r\n", b"\n"):
                break
            key, _, value = line.decode("ascii").partition(":")
            headers[key.strip().lower()] = value.strip()
        # 2) Read exactly Content-Length bytes for the body. Don't readline()
        #    here — the body is binary-framed JSON, not line-oriented.
        length = int(headers["content-length"])
        body = _read_exact(self.proc.stdout, length)
        payload = json.loads(body.decode("utf-8"))
        header_bytes = f"Content-Length: {length}\r\n\r\n".encode("ascii")
        _dump("← RECV", header_bytes, payload)
        return payload


def _read_exact(stream, n: int) -> bytes:
    """`stream.read(n)` is allowed to return fewer than n bytes — loop until we have them all."""
    chunks: list[bytes] = []
    remaining = n
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            raise RuntimeError("server closed stdout mid-frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def _dump(label: str, header: bytes, payload: dict) -> None:
    """Pretty-print one frame so the protocol is readable in the terminal."""
    bar = "─" * 60
    print(f"\n{label} {bar}")
    sys.stdout.write(header.decode("ascii"))
    print(json.dumps(payload, indent=2))
    sys.stdout.flush()


def _pump_stderr(proc: subprocess.Popen[bytes]) -> None:
    """Mirror the server's stderr (ty's own logs) so we see context, not just protocol."""
    assert proc.stderr is not None
    for line in iter(proc.stderr.readline, b""):
        sys.stderr.write(f"[ty stderr] {line.decode('utf-8', errors='replace')}")


def _did_open(framer: Framer, path: Path) -> None:
    """Send textDocument/didOpen for `path`, handing ty its current text.

    LSP servers do NOT read files from disk — the client owns buffer state.
    didOpen starts the server tracking the file and (for ty) triggers an
    unprompted publishDiagnostics. Crucially for rename: a reference in another
    file is only found if that file is also open, so we open both.
    """
    framer.send({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": path.as_uri(),
                "languageId": "python",
                "version": 1,
                "text": path.read_text(),
            },
        },
    })


def _wait_for_id(framer: Framer, request_id: int, timeout: float = 10.0) -> dict:
    """Drain frames until we see the response matching `request_id`.

    The server is free to interleave notifications (e.g. publishDiagnostics,
    window/logMessage, $/progress) between our request and its response, so
    we can't assume the next frame is the one we want.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        msg = framer.recv()
        if msg.get("id") == request_id:
            return msg
    raise TimeoutError(f"no response for id={request_id} within {timeout}s")


# ─── Applying a WorkspaceEdit ─────────────────────────────────────────────────
# A rename (or code action) returns a WorkspaceEdit — the structured edit list
# a tool *applies*. Doing that correctly is the whole game for an LLM rename
# tool, and there are exactly two traps, both handled below:
#
#   1. UTF-16 offsets. ty advertised `positionEncoding: utf-16`, so a Range's
#      `character` counts UTF-16 code units, NOT Python code points. They're
#      equal for ASCII but diverge on any astral-plane char (emoji, etc.): a
#      single emoji is 1 Python code point but 2 UTF-16 units. Splice using raw
#      `character` values and every edit after an emoji on that line is off.
#
#   2. Offset drift. Each splice changes the length of the text, shifting the
#      offsets of everything after it. Applying edits top-to-bottom corrupts
#      every later range. Apply bottom-to-top (descending by start) instead —
#      LSP guarantees a document's edits don't overlap, so this is always safe.
#
# A WorkspaceEdit also has two encodings: the plain `changes` map ty returns,
# and the richer `documentChanges` (versioned files + Create/Rename/Delete
# resource ops). A real tool handles both; this one applies text edits from
# either and refuses resource ops loudly rather than silently dropping them.


def _utf16_to_codepoint_index(line: str, utf16_col: int) -> int:
    """Convert a UTF-16 code-unit column into a Python (code-point) index.

    Walks the line counting UTF-16 units — 2 for chars above U+FFFF, 1 otherwise
    — and returns the Python index once the count reaches `utf16_col`.
    """
    units = 0
    for idx, ch in enumerate(line):
        if units >= utf16_col:
            return idx
        units += 2 if ord(ch) > 0xFFFF else 1
    return len(line)


def _line_start_offsets(text: str) -> list[int]:
    """Code-point index where each line begins, by LSP line numbering.

    LSP treats `\\r\\n`, `\\r`, and `\\n` (and only those) as line terminators.
    Index `i` of the result is where line `i`'s content starts.
    """
    offsets = [0]
    i, n = 0, len(text)
    while i < n:
        ch = text[i]
        if ch == "\r":
            i += 2 if i + 1 < n and text[i + 1] == "\n" else 1
            offsets.append(i)
        elif ch == "\n":
            i += 1
            offsets.append(i)
        else:
            i += 1
    return offsets


def _position_to_offset(text: str, line_starts: list[int], position: dict) -> int:
    """Resolve an LSP Position {line, character} to a flat code-point offset."""
    line = position["line"]
    if line >= len(line_starts):
        return len(text)
    line_start = line_starts[line]
    line_end = line_starts[line + 1] if line + 1 < len(line_starts) else len(text)
    line_text = text[line_start:line_end]
    return line_start + _utf16_to_codepoint_index(line_text, position["character"])


def apply_text_edits(text: str, edits: list[dict]) -> str:
    """Apply a list of LSP TextEdits to `text` and return the new text.

    Pure (no I/O) so it's easy to test. Edits are applied bottom-to-top so an
    earlier splice never invalidates a later edit's offsets.
    """
    line_starts = _line_start_offsets(text)
    resolved = [
        (
            _position_to_offset(text, line_starts, e["range"]["start"]),
            _position_to_offset(text, line_starts, e["range"]["end"]),
            e["newText"],
        )
        for e in edits
    ]
    for start, end, new_text in sorted(resolved, key=lambda r: r[0], reverse=True):
        text = text[:start] + new_text + text[end:]
    return text


def _uri_to_path(uri: str) -> Path:
    """Turn a `file://` URI into a local Path (decoding %xx escapes)."""
    return Path(unquote(urlparse(uri).path))


def collect_text_edits(workspace_edit: dict) -> dict[str, list[dict]]:
    """Flatten a WorkspaceEdit to {uri: TextEdit[]}, from either encoding.

    Prefers `documentChanges` when present (the spec says clients that support
    it must). Raises on Create/Rename/Delete resource operations rather than
    silently ignoring them — this demo only does in-place text edits.
    """
    doc_changes = workspace_edit.get("documentChanges")
    if doc_changes is not None:
        result: dict[str, list[dict]] = {}
        for change in doc_changes:
            kind = change.get("kind")
            if kind in ("create", "rename", "delete"):
                raise NotImplementedError(
                    f"WorkspaceEdit resource operation {kind!r} not supported"
                )
            uri = change["textDocument"]["uri"]
            result.setdefault(uri, []).extend(change["edits"])
        return result
    return {uri: list(edits) for uri, edits in workspace_edit.get("changes", {}).items()}


def apply_workspace_edit(workspace_edit: dict) -> dict[Path, str]:
    """Apply a WorkspaceEdit to files on disk. Returns {path: new_text}."""
    results: dict[Path, str] = {}
    for uri, edits in collect_text_edits(workspace_edit).items():
        path = _uri_to_path(uri)
        new_text = apply_text_edits(path.read_text(), edits)
        path.write_text(new_text)
        results[path] = new_text
    return results


def _dump_file(path: Path, text: str) -> None:
    """Print a file's post-edit contents, in the same banner style as frames."""
    bar = "─" * 60
    print(f"\n✎ APPLIED → {path.name} {bar}")
    sys.stdout.write(text if text.endswith("\n") else text + "\n")
    sys.stdout.flush()


# ─── Conversation ───────────────────────────────────────────────────────────
# Every LSP session follows the same lifecycle:
#   1.  initialize (request)        — handshake; server returns its capabilities
#   2.  initialized (notification)  — client says "ready"; server may now push
#   3.  ...work...                  — textDocument/* and workspace/* methods
#   4.  shutdown (request)          — graceful stop signal
#   5.  exit (notification)         — actually terminate
#
# Steps 1, 2, 4, 5 are mandatory. Skip them and behavior is undefined.

def main() -> None:
    # Spawn ty's LSP via uv so it picks up the project's pinned ty version.
    # stdin/stdout/stderr are all piped so we control both directions of the
    # protocol and capture ty's own logging without it polluting the protocol
    # stream (ty logs to stderr, protocol is on stdout — same as every LSP).
    proc = subprocess.Popen(
        ["uv", "run", "ty", "server"],
        cwd=PROJECT_ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    threading.Thread(target=_pump_stderr, args=(proc,), daemon=True).start()
    framer = Framer(proc)

    # 1) initialize — REQUEST.
    # We declare who we are, where the workspace is, and which protocol
    # features we (claim to) support. The server's response is its own
    # capability advertisement — the menu of what it can do for us.
    init_id = framer.new_id()
    framer.send({
        "jsonrpc": "2.0",
        "id": init_id,
        "method": "initialize",
        "params": {
            "processId": None,
            "clientInfo": {"name": "lsp-raw-client", "version": "0.0.1"},
            "rootUri": PROJECT_ROOT.as_uri(),
            "capabilities": {
                "textDocument": {
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "publishDiagnostics": {"relatedInformation": True},
                    "synchronization": {"didSave": True},
                    # Declaring prepareSupport tells the server we'll call
                    # prepareRename before rename. ty advertises
                    # renameProvider.prepareProvider regardless, but a correct
                    # client states what it intends to use.
                    "rename": {"prepareSupport": True},
                },
            },
            "workspaceFolders": [
                {"uri": PROJECT_ROOT.as_uri(), "name": PROJECT_ROOT.name},
            ],
        },
    })
    _wait_for_id(framer, init_id)

    # 2) initialized — NOTIFICATION (no `id`, no response expected).
    # Before this, the server must not send any unsolicited messages. After,
    # the floodgates open: diagnostics, progress, log messages, etc.
    framer.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    # 3a) textDocument/didOpen — NOTIFICATION (one per file).
    # We open BOTH files: sample.py (defines greet) and consumer.py (imports
    # and calls it). Opening consumer.py is what lets the later rename reach
    # across files. Each open triggers its own publishDiagnostics — sample.py
    # reports the deliberate type error, consumer.py reports none.
    _did_open(framer, SAMPLE_FILE)
    _did_open(framer, CONSUMER_FILE)

    # 3b) textDocument/hover — REQUEST.
    # Positions are zero-indexed (line, character). The character offset is
    # measured in UTF-16 code units by default — ty advertised utf-16 in the
    # initialize response. For ASCII files like ours this matches byte/char
    # counts, but it matters once non-BMP characters are in the file.
    # Position (5, 10) lands on the `greet` call in `message = greet(123)`
    # (line 5 now that a decoy comment sits inside the function body).
    hover_id = framer.new_id()
    framer.send({
        "jsonrpc": "2.0",
        "id": hover_id,
        "method": "textDocument/hover",
        "params": {
            "textDocument": {"uri": SAMPLE_FILE.as_uri()},
            "position": {"line": 5, "character": 10},
        },
    })
    _wait_for_id(framer, hover_id)

    # 3c) textDocument/prepareRename — REQUEST.
    # Before renaming, a well-behaved client asks: "is this position even a
    # rename target, and what span would I be renaming?" ty replies with the
    # Range of the symbol under the cursor — here `greet` on line 5, chars
    # 10–15 — or `null` if the position isn't renameable (whitespace, a
    # keyword, a literal like `123`). Two uses: (a) decide whether to offer
    # rename at all, and (b) pre-select the exact span so the edit box
    # highlights `greet`, not `greet(123)`. For an LLM tool this is the cheap
    # validity probe before committing to the heavier rename request — call it
    # first and bail if it returns null.
    prepare_id = framer.new_id()
    framer.send({
        "jsonrpc": "2.0",
        "id": prepare_id,
        "method": "textDocument/prepareRename",
        "params": {
            "textDocument": {"uri": SAMPLE_FILE.as_uri()},
            "position": {"line": 5, "character": 10},
        },
    })
    _wait_for_id(framer, prepare_id)

    # 3d) textDocument/rename — REQUEST.
    # The payload that matters: a WorkspaceEdit. We send the position plus the
    # desired newName; ty returns a structured description of *every* edit
    # needed to rename safely, keyed by file URI — each a list of
    # {range, newText}. Watch what the server does that text search/replace
    # cannot:
    #   • It edits real references across BOTH files — the `def greet` and the
    #     `greet(123)` call in sample.py, plus the `import greet` and the
    #     `greet("world")` call in consumer.py — in one WorkspaceEdit.
    #   • It does NOT touch the word "greet" sitting in sample.py's decoy
    #     comment or its f-string, because those aren't references to the
    #     symbol.
    # That semantic scoping is the whole reason to drive renames through the
    # LSP. An LLM should *apply* this WorkspaceEdit verbatim, never
    # hand-synthesize the edit list.
    rename_id = framer.new_id()
    framer.send({
        "jsonrpc": "2.0",
        "id": rename_id,
        "method": "textDocument/rename",
        "params": {
            "textDocument": {"uri": SAMPLE_FILE.as_uri()},
            "position": {"line": 5, "character": 10},
            "newName": "salutation",
        },
    })
    rename_msg = _wait_for_id(framer, rename_id)

    # 3e) Apply the WorkspaceEdit — the round-trip the rename existed for.
    # apply_workspace_edit() writes the edited files to disk; we then print
    # them and immediately restore the originals so the script stays
    # re-runnable. The snapshot/restore is a testbed convenience — the apply
    # itself is real (the files genuinely change on disk in between).
    snapshot = {path: path.read_text() for path in (SAMPLE_FILE, CONSUMER_FILE)}
    try:
        for path, new_text in apply_workspace_edit(rename_msg["result"]).items():
            _dump_file(path, new_text)
    finally:
        for path, original in snapshot.items():
            path.write_text(original)

    # 4) shutdown — REQUEST. Server stops doing work but stays alive.
    # 5) exit    — NOTIFICATION. Server process terminates.
    # Two steps so a client can confirm graceful stop (shutdown returns) before
    # killing the process. Skipping shutdown is allowed but the spec says the
    # server should exit with code 1; with shutdown first, it exits 0.
    shutdown_id = framer.new_id()
    framer.send({"jsonrpc": "2.0", "id": shutdown_id, "method": "shutdown"})
    _wait_for_id(framer, shutdown_id)
    framer.send({"jsonrpc": "2.0", "method": "exit"})

    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


if __name__ == "__main__":
    main()
