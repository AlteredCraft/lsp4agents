"""Tests for the WorkspaceEdit apply logic in lsp_raw_client.

The risky parts are pure functions (no LSP, no subprocess), so they're cheap to
test directly. The two correctness traps — UTF-16 offsets and edit-order drift —
get dedicated cases.

Run with: `uv run pytest`
"""

from __future__ import annotations

import pytest

from lsp_raw_client import (
    _line_start_offsets,
    _utf16_to_codepoint_index,
    apply_text_edits,
    apply_workspace_edit,
    collect_text_edits,
)


def _edit(sl: int, sc: int, el: int, ec: int, new_text: str) -> dict:
    """Build a TextEdit from start/end (line, character) and replacement text."""
    return {
        "range": {"start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec}},
        "newText": new_text,
    }


@pytest.mark.parametrize(
    "line, utf16_col, expected_index",
    [
        ("hello", 0, 0),       # ASCII: UTF-16 col == code-point index
        ("hello", 5, 5),       # end of an ASCII line
        ("a😀bc", 0, 0),       # before the astral char
        ("a😀bc", 1, 1),       # the emoji itself
        ("a😀bc", 3, 2),       # 'b' — emoji ate 2 UTF-16 units but 1 index
        ("a😀bc", 5, 4),       # end of line: 5 UTF-16 units, 4 code points
        ("😀😀", 4, 2),        # two emoji = 4 UTF-16 units, 2 code points
    ],
)
def test_utf16_to_codepoint_index(line: str, utf16_col: int, expected_index: int) -> None:
    assert _utf16_to_codepoint_index(line, utf16_col) == expected_index


@pytest.mark.parametrize(
    "text, expected",
    [
        ("a\nb\nc", [0, 2, 4]),          # LF
        ("a\r\nb\r\nc", [0, 3, 6]),      # CRLF — \r\n is one terminator
        ("a\rb\rc", [0, 2, 4]),          # bare CR
        ("no newline", [0]),             # single line
        ("trailing\n", [0, 9]),          # terminator starts an (empty) next line
    ],
)
def test_line_start_offsets(text: str, expected: list[int]) -> None:
    assert _line_start_offsets(text) == expected


def test_single_edit_ascii() -> None:
    text = "message = greet(123)\n"
    # Replace `greet` (chars 10–15 on line 0) with `salutation`.
    assert apply_text_edits(text, [_edit(0, 10, 0, 15, "salutation")]) == (
        "message = salutation(123)\n"
    )


def test_edits_applied_bottom_to_top() -> None:
    # Two edits of *different* lengths on one line. Applied top-to-bottom the
    # first splice would shift the second edit's offsets and corrupt it; the
    # function must apply bottom-to-top regardless of input order.
    text = "abcdef"
    edits = [_edit(0, 0, 0, 3, ""), _edit(0, 4, 0, 6, "Z")]  # delete "abc", "ef"->"Z"
    assert apply_text_edits(text, edits) == "dZ"
    assert apply_text_edits(text, list(reversed(edits))) == "dZ"  # order-independent


def test_utf16_offsets_with_astral_char() -> None:
    # The headline trap: an emoji before the edit shifts UTF-16 columns past the
    # Python index. `bc` is UTF-16 [3, 5) but Python [2, 4).
    assert apply_text_edits("a😀bc", [_edit(0, 3, 0, 5, "Z")]) == "a😀Z"


def test_multiline_edit() -> None:
    text = "x = 1\ny = greet(2)\nz = 3\n"
    # Rename `greet` on line 1 (chars 4–9).
    assert apply_text_edits(text, [_edit(1, 4, 1, 9, "hi")]) == "x = 1\ny = hi(2)\nz = 3\n"


def test_collect_from_changes() -> None:
    we = {"changes": {"file:///tmp/a.py": [_edit(0, 0, 0, 1, "X")]}}
    assert collect_text_edits(we) == {"file:///tmp/a.py": [_edit(0, 0, 0, 1, "X")]}


def test_collect_prefers_document_changes() -> None:
    we = {
        # documentChanges present → `changes` must be ignored.
        "changes": {"file:///ignored.py": [_edit(0, 0, 0, 1, "no")]},
        "documentChanges": [
            {
                "textDocument": {"uri": "file:///tmp/a.py", "version": 1},
                "edits": [_edit(0, 0, 0, 1, "X")],
            },
        ],
    }
    assert collect_text_edits(we) == {"file:///tmp/a.py": [_edit(0, 0, 0, 1, "X")]}


@pytest.mark.parametrize("kind", ["create", "rename", "delete"])
def test_collect_rejects_resource_operations(kind: str) -> None:
    we = {"documentChanges": [{"kind": kind, "uri": "file:///tmp/new.py"}]}
    with pytest.raises(NotImplementedError):
        collect_text_edits(we)


def test_apply_workspace_edit_to_disk(tmp_path) -> None:
    a = tmp_path / "a.py"
    b = tmp_path / "b.py"
    a.write_text("x = greet(1)\n")
    b.write_text("y = greet(2)\n")
    workspace_edit = {
        "changes": {
            a.as_uri(): [_edit(0, 4, 0, 9, "hi")],
            b.as_uri(): [_edit(0, 4, 0, 9, "hi")],
        }
    }
    results = apply_workspace_edit(workspace_edit)
    assert a.read_text() == "x = hi(1)\n"
    assert b.read_text() == "y = hi(2)\n"
    assert set(results) == {a, b}
