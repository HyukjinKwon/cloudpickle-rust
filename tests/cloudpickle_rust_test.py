"""Tests specific to the Rust implementation of cloudpickle.

The rest of the test-suite is the upstream cloudpickle test-suite. The tests of
this module check the parts of cloudpickle-rust that differ in implementation
from upstream: the native decoding of the bytecode, and the equivalence of the
produced payloads and of the module namespace with upstream cloudpickle (when
the matching version of cloudpickle is installed, see dev-requirements.txt).
"""

import abc
import collections
import dataclasses
import dis
import enum
import functools
import importlib
import importlib.metadata
import logging
import os
import pickle
import pickletools
import re
import sys
import types
import typing

import pytest

import cloudpickle_rust
from cloudpickle_rust.cloudpickle import _walk_global_ops, GLOBAL_OPS

# The commit of cloudpickle implemented by this version of cloudpickle-rust.
UPSTREAM_VERSION = "3.2.0.dev0"

GLOBAL_VARIABLE = 42


def _upstream_cloudpickle():
    try:
        import cloudpickle
    except Exception as e:  # noqa
        # Including upstream failing at import time (e.g. on PyPy 3.11).
        pytest.skip(f"upstream cloudpickle is not available: {e!r}")
    if cloudpickle.__version__ != UPSTREAM_VERSION:
        pytest.skip(
            f"upstream cloudpickle {cloudpickle.__version__} is installed, "
            f"the parity tests need cloudpickle {UPSTREAM_VERSION}"
        )
    return cloudpickle


def _all_code_objects(modules):
    seen = set()
    stack = []
    for module in modules:
        for value in vars(module).values():
            if isinstance(value, types.FunctionType):
                stack.append(value.__code__)
            elif isinstance(value, type):
                for attr in vars(value).values():
                    if isinstance(attr, (staticmethod, classmethod)):
                        attr = attr.__func__
                    if isinstance(attr, types.FunctionType):
                        stack.append(attr.__code__)
    while stack:
        code = stack.pop()
        # PyPy's builtin functions have "builtin-code" objects.
        if not isinstance(code, types.CodeType) or id(code) in seen:
            continue
        seen.add(id(code))
        yield code
        stack.extend(c for c in code.co_consts if isinstance(c, types.CodeType))


def test_walk_global_ops_matches_dis():
    # cloudpickle-rust decodes the bytecode directly instead of relying on
    # dis.get_instructions: check that both agree on many code objects.
    module_names = [
        "abc",
        "argparse",
        "ast",
        "asyncio.base_events",
        "collections",
        "concurrent.futures.process",
        "dataclasses",
        "dis",
        "email.message",
        "enum",
        "functools",
        "http.client",
        "inspect",
        "json.decoder",
        "logging",
        "os",
        "pathlib",
        "pickle",
        "pydoc",
        "re",
        "shutil",
        "subprocess",
        "tarfile",
        "threading",
        "typing",
        "unittest.case",
        "urllib.parse",
        "uuid",
        "zipfile",
    ]
    modules = []
    for name in module_names:
        try:
            modules.append(importlib.import_module(name))
        except ImportError:
            pass
    n_codes = 0
    for code in _all_code_objects(modules):
        expected = [
            instr.argval
            for instr in dis.get_instructions(code)
            if instr.opcode in GLOBAL_OPS
        ]
        assert list(_walk_global_ops(code)) == expected, code
        n_codes += 1
    assert n_codes > 1000


def test_walk_global_ops_extended_arg():
    names = [f"g{i}" for i in range(300)]
    code = compile(
        "def f():\n    return ({},)\n".format(", ".join(names)), "<f>", "exec"
    )
    (func_code,) = [c for c in code.co_consts if isinstance(c, types.CodeType)]
    assert list(_walk_global_ops(func_code)) == names


def test_version():
    try:
        installed = importlib.metadata.version("cloudpickle-rust")
    except importlib.metadata.PackageNotFoundError:
        pytest.skip("cloudpickle-rust is not installed")
    assert cloudpickle_rust.__version__ == installed


def test_module_namespace_matches_upstream():
    cloudpickle = _upstream_cloudpickle()
    # __warningregistry__ is created when a warning is emitted by the module.
    lazy = {"__warningregistry__"}
    upstream_names = set(vars(cloudpickle.cloudpickle)) - {"__cached__"} - lazy
    # DYNAMIC_CODE_FILENAME is an extension of cloudpickle-rust.
    extensions = {"_DispatchTable", "DYNAMIC_CODE_FILENAME"}
    rust_names = set(vars(cloudpickle_rust.cloudpickle)) - extensions - lazy
    assert upstream_names == rust_names
    assert set(vars(cloudpickle)) == set(vars(cloudpickle_rust))
    assert cloudpickle.__all__ == cloudpickle_rust.__all__


_UUID = re.compile(r"^[0-9a-f]{32}$")


def _normalized_opcodes(payload):
    """Opcodes of a payload, modulo the module of the reconstructors, the
    frame sizes and the (random) class tracker ids."""
    ops = []
    tracker_ids = {}
    for opcode, arg, _ in pickletools.genops(payload):
        if opcode.name == "FRAME":
            arg = None
        elif isinstance(arg, str):
            if arg == "cloudpickle_rust.cloudpickle":
                arg = "cloudpickle.cloudpickle"
            elif arg.startswith("cloudpickle_rust.cloudpickle "):
                arg = "cloudpickle.cloudpickle " + arg.split(" ", 1)[1]
            elif _UUID.match(arg):
                arg = tracker_ids.setdefault(arg, f"<tracker id {len(tracker_ids)}>")
        ops.append((opcode.name, arg))
    return ops


def _parity_objects():
    def closure_factory():
        a, b = 1, [1, 2]

        def inner(c, *args, d=2, **kwargs) -> int:
            """docstring"""
            return a + len(b) + c + d + GLOBAL_VARIABLE

        inner.attribute = {"x": 1}
        return inner

    def recursive(n):
        return 1 if n <= 1 else n * recursive(n - 1)

    def uses_submodule():
        return os.path.join("a", "b"), collections.abc.Mapping

    T = typing.TypeVar("T", bound=int)

    class Base(abc.ABC):
        attribute = "base"

        @abc.abstractmethod
        def method(self):
            pass

    class Derived(Base, typing.Generic[T]):
        """Derived docstring"""

        __slots__ = ("x",)

        def method(self):
            return super().method()

        @classmethod
        def cm(cls):
            return cls

        @staticmethod
        def sm():
            return 1

        @property
        def prop(self):
            return 2

    Base.register(tuple)

    @dataclasses.dataclass
    class DataClass:
        x: int
        y: dataclasses.InitVar[int] = None
        z: typing.ClassVar[int] = 3

    class Color(enum.Enum):
        RED = 1
        GREEN = 2

    module = types.ModuleType("dynamic_module")
    exec("x = 1\ndef f(y):\n    return x + y\n", vars(module))

    d = {"a": 1}
    return [
        lambda x: x + GLOBAL_VARIABLE,
        closure_factory(),
        recursive,
        uses_submodule,
        functools.partial(recursive, 3),
        T,
        typing.AnyStr,
        Base,
        Derived,
        Derived[int],
        DataClass,
        DataClass(1),
        Color,
        Color.RED,
        module,
        logging.getLogger("cloudpickle_rust.test"),
        memoryview(b"abc"),
        types.MappingProxyType(d),
        d.keys(),
        collections.OrderedDict(d).items(),
        float.real,
        type(None),
        types.CodeType,
        closure_factory.__code__,
        [closure_factory(), closure_factory()],
    ]


@pytest.mark.parametrize("protocol", [2, 3, 4, 5])
def test_payloads_identical_to_upstream(protocol):
    cloudpickle = _upstream_cloudpickle()
    for obj in _parity_objects():
        expected = _normalized_opcodes(cloudpickle.dumps(obj, protocol=protocol))
        payload = cloudpickle_rust.dumps(obj, protocol=protocol)
        assert _normalized_opcodes(payload) == expected, obj
        pickle.loads(payload)


def _function_with_nested_code():
    def outer(x):
        def inner(y):
            return y + x

        return [inner(i) for i in range(x)]

    return outer


def _code_objects(func):
    """The code of func and the code objects that it contains."""
    codes = []
    stack = [func.__code__]
    while stack:
        code = stack.pop()
        codes.append(code)
        stack.extend(c for c in code.co_consts if isinstance(c, types.CodeType))
    return codes


def test_dynamic_code_filename_default():
    assert cloudpickle_rust.cloudpickle.DYNAMIC_CODE_FILENAME == "<dynamic-code>"
    # Only the module attribute is looked up, the package does not expose a copy.
    assert not hasattr(cloudpickle_rust, "DYNAMIC_CODE_FILENAME")
    outer = pickle.loads(cloudpickle_rust.dumps(_function_with_nested_code()))
    assert {c.co_filename for c in _code_objects(outer)} == {"<dynamic-code>"}
    assert outer(2) == [2, 3]


def test_dynamic_code_filename_none_keeps_the_filename(monkeypatch):
    monkeypatch.setattr(cloudpickle_rust.cloudpickle, "DYNAMIC_CODE_FILENAME", None)
    func = _function_with_nested_code()
    filename = func.__code__.co_filename
    outer = pickle.loads(cloudpickle_rust.dumps(func))
    codes = _code_objects(outer)
    assert len(codes) > 1
    assert {c.co_filename for c in codes} == {filename}
    assert outer(2) == [2, 3]
    shared = all(c.co_filename is filename for c in _code_objects(func))
    if shared and sys.implementation.name == "cpython":
        # As in cloudpickle 3.1, code objects that share their filename still
        # share it once unpickled: it is pickled once. (PyPy 7.3 does not keep
        # the identity of the strings of code objects.)
        assert all(c.co_filename is codes[0].co_filename for c in codes)
    for obj in _parity_objects():
        pickle.loads(cloudpickle_rust.dumps(obj))


def test_dynamic_code_filename_custom(monkeypatch):
    monkeypatch.setattr(
        cloudpickle_rust.cloudpickle, "DYNAMIC_CODE_FILENAME", "<remote>"
    )
    outer = pickle.loads(cloudpickle_rust.dumps(_function_with_nested_code()))
    assert {c.co_filename for c in _code_objects(outer)} == {"<remote>"}


def test_dynamic_code_filename_must_be_a_str_or_none(monkeypatch):
    monkeypatch.setattr(cloudpickle_rust.cloudpickle, "DYNAMIC_CODE_FILENAME", 1)
    with pytest.raises(TypeError, match="must be a str or None, not int"):
        cloudpickle_rust.dumps(_function_with_nested_code())


def test_pickler_methods_accept_keyword_arguments():
    # The methods of the Pickler are implemented in Rust but keep the
    # signatures of the upstream Python methods.
    import io

    buffer = io.BytesIO()
    pickler = cloudpickle_rust.Pickler(file=buffer, protocol=None, buffer_callback=None)
    pickler.dump(obj=lambda: 1)
    assert pickle.loads(buffer.getvalue())() == 1
    assert pickler.reducer_override(obj=1) is NotImplemented
    with pytest.raises(
        TypeError, match="missing 1 required positional argument: 'file'"
    ):
        cloudpickle_rust.Pickler()
    with pytest.raises(TypeError, match="unexpected keyword argument 'fix_imports'"):
        cloudpickle_rust.Pickler(buffer, fix_imports=True)
    with pytest.raises(TypeError, match="multiple values for argument 'protocol'"):
        cloudpickle_rust.Pickler(buffer, 2, protocol=2)


def test_pickler_subclass_super_calls():
    import io

    class MyPickler(cloudpickle_rust.Pickler):
        def __init__(self, file):
            super().__init__(file, protocol=4)
            self.seen = []

        def reducer_override(self, obj):
            self.seen.append(type(obj))
            return super().reducer_override(obj)

    buffer = io.BytesIO()
    pickler = MyPickler(buffer)
    pickler.dump([lambda: 2, logging.getLogger("x")])
    assert pickler.proto == 4
    assert types.FunctionType in pickler.seen
    f, logger = pickle.loads(buffer.getvalue())
    assert f() == 2
    assert logger is logging.getLogger("x")


def test_dispatch_table_is_a_chainmap():
    dispatch_table = cloudpickle_rust.Pickler.dispatch_table
    assert isinstance(dispatch_table, collections.ChainMap)
    assert dispatch_table.maps[0] is cloudpickle_rust.Pickler._dispatch_table
    assert dispatch_table[types.CodeType] is cloudpickle_rust.cloudpickle._code_reduce
    with pytest.raises(KeyError):
        dispatch_table[int]
    assert dispatch_table.get(int) is None
    child = dispatch_table.new_child({int: str})
    assert child[int] is str
    assert child[types.CodeType] is cloudpickle_rust.cloudpickle._code_reduce


@pytest.mark.skipif(
    not hasattr(sys, "_is_gil_enabled"), reason="free-threaded builds only"
)
def test_gil_is_not_enabled_by_import():
    import subprocess

    code = "import sys, cloudpickle_rust; print(sys._is_gil_enabled())"
    out = subprocess.check_output([sys.executable, "-c", code])
    gil_enabled_by_default = subprocess.check_output(
        [sys.executable, "-c", "import sys; print(sys._is_gil_enabled())"]
    )
    assert out == gil_enabled_by_default
