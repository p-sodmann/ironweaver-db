"""The Python examples of the documentation run (step 17d).

Every ```python block of the README, documentation/python-api.md and
documentation/concepts.md runs, page by page, in order, in a fresh
directory, with one namespace of globals per page (a block may use what an
earlier one defined). A block that doesn't open a store itself runs with
`store` open on the page's "data". Blocks that need a server (`connect`)
are compiled, not run.
"""

import ast
import os
import pathlib
import textwrap

import pytest

import iwdb

ROOT = pathlib.Path(__file__).resolve().parents[3]
PAGES = ["README.md", "documentation/python-api.md", "documentation/concepts.md"]


def python_blocks(text):
    """The ```python blocks of a Markdown text, dedented (a block may sit in a list)."""
    blocks, current = [], None
    for line in text.splitlines():
        fence = line.strip().startswith("```")
        if current is None and fence and line.strip()[3:].strip().startswith("python"):
            current = []
        elif current is not None and fence:
            blocks.append(textwrap.dedent("\n".join(current)))
            current = None
        elif current is not None:
            current.append(line)
    return blocks


def test_blocks_are_found_in_lists_too():
    text = "a\n```python\nx = 1\n```\n- item\n\n  ```python\n  y = 2\n  ```\n```sh\nz\n```\n"
    assert python_blocks(text) == ["x = 1", "y = 2"]


@pytest.mark.parametrize("page", PAGES)
def test_the_python_examples_run(page, tmp_path, monkeypatch):
    blocks = python_blocks((ROOT / page).read_text(encoding="utf-8"))
    assert blocks, f"no python block in {page}"
    monkeypatch.chdir(tmp_path)
    scope = {"iwdb": iwdb, "os": os, "__name__": "__docs__"}
    for block in blocks:
        if "connect(" in block:
            # Needs a server: it must at least be valid Python
            compile(block, page, "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
            continue
        code = compile(block, page, "exec")
        if "Store.open(" in block:
            exec(code, scope)
        else:
            with iwdb.Store.open("data") as store:
                scope["store"] = store
                exec(code, scope)
