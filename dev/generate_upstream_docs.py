"""Generate src/upstream_docs.rs from src/upstream/cloudpickle.py.

The docstrings of upstream's functions and of the methods of its Pickler
class (both implementations), as the values of their literals (the compiler
of Python 3.13 and later also strips their indentation: cloudpickle-rust
compiles them the same way). Run from the root of the repository:

    python dev/generate_upstream_docs.py
"""

import ast
import os

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), os.pardir)
SOURCE = os.path.join(ROOT, "src", "upstream", "cloudpickle.py")
TARGET = os.path.join(ROOT, "src", "upstream_docs.rs")


def rust_str(s):
    out = []
    for ch in s:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ord(ch) < 0x20 or ord(ch) > 0x7E:
            out.append("\\u{%x}" % ord(ch))
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def definitions(nodes):
    """The function definitions of a module or class body (in all branches)."""
    for node in nodes:
        if isinstance(node, ast.FunctionDef):
            yield node
        elif isinstance(node, ast.If):
            yield from definitions(node.body)
            yield from definitions(node.orelse)


def main():
    with open(SOURCE, encoding="utf-8") as f:
        module = ast.parse(f.read())
    functions = {}
    for node in definitions(module.body):
        # _getattribute is defined twice (for different Python versions),
        # without docstring.
        functions.setdefault(node.name, ast.get_docstring(node, clean=False))
    pickler = next(
        node
        for node in module.body
        if isinstance(node, ast.ClassDef) and node.name == "Pickler"
    )
    methods = {
        node.name: ast.get_docstring(node, clean=False)
        for node in definitions(pickler.body)
    }
    lines = [
        "//! The docstrings of upstream's functions and of the methods of its",
        "//! `Pickler` class, verbatim (generated from `src/upstream/cloudpickle.py`",
        "//! by `dev/generate_upstream_docs.py`, checked by",
        "//! `tests/cloudpickle_rust_test.py`). They are the values of the docstring",
        "//! literals: the compiler of Python 3.13 and later also strips their",
        "//! indentation, see `functions::compiled_doc`.",
        "",
        "/// `(name, docstring)` of the functions of the module.",
        "pub(crate) const FUNCTIONS: &[(&str, &str)] = &[",
    ]
    for name in sorted(functions):
        if functions[name] is not None:
            lines.append(f"    ({rust_str(name)}, {rust_str(functions[name])}),")
    lines += [
        "];",
        "",
        "/// `(name, docstring)` of the methods of `Pickler` (both implementations).",
        "pub(crate) const PICKLER_METHODS: &[(&str, &str)] = &[",
    ]
    for name in sorted(methods):
        if methods[name] is not None:
            lines.append(f"    ({rust_str(name)}, {rust_str(methods[name])}),")
    lines.append("];")
    with open(TARGET, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
