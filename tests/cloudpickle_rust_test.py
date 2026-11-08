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
    assert pickler._function_reduce(obj=os.path.join) is NotImplemented
    if hasattr(pickler, "reducer_override"):  # not on PyPy, like upstream
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

        def _dynamic_function_reduce(self, func):
            self.seen.append(type(func))
            return super()._dynamic_function_reduce(func)

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


_DOCSTRINGS_SCRIPT = """
import json, pickle, types
import {module} as cp

mod = cp.cloudpickle
docs = {{"<package>": cp.__doc__, "<module>": mod.__doc__}}
for name, value in vars(mod).items():
    if isinstance(value, types.FunctionType) and value.__module__ == mod.__name__:
        docs[name] = value.__doc__
for name, value in vars(mod.Pickler).items():
    if callable(value) and not isinstance(value, type):
        docs["Pickler." + name] = value.__doc__


class C:
    def method(self):
        return 2


assert pickle.loads(cp.dumps(lambda x: x + C().method()))(1) == 3
print(json.dumps(docs))
"""


def _docstrings(module, flags):
    """The docstrings of module (cloudpickle or cloudpickle_rust) and of its
    functions and Pickler methods, in a Python process run with flags."""
    import json
    import subprocess

    script = _DOCSTRINGS_SCRIPT.format(module=module)
    return json.loads(subprocess.check_output([sys.executable, *flags, "-c", script]))


@pytest.mark.parametrize("flags", [(), ("-O",), ("-OO",)])
def test_docstrings_with_optimizations(flags):
    # python -OO removes the docstrings: cloudpickle-rust must still import
    # (0.3.0 did not) and, like upstream's, its functions and methods then
    # have no docstring.
    docs = _docstrings("cloudpickle_rust", flags)
    assert len(docs) > 60
    if flags == ("-OO",):
        assert all(doc is None for doc in docs.values()), docs
    else:
        assert docs["dumps"].startswith("Serialize obj as a string of bytes")
    _upstream_cloudpickle()
    expected = _docstrings("cloudpickle", flags)
    if flags != ("-OO",):
        # The docstring of the module also states the license of
        # cloudpickle-rust.
        for docstrings in (docs, expected):
            del docstrings["<package>"], docstrings["<module>"]
    assert docs == expected


# ---------------------------------------------------------------------------
# Call-time lookups of the globals: the functions of upstream's module read
# its globals (and the attributes of the modules it imported) when they run,
# so replacing them changes the behavior of cloudpickle. cloudpickle-rust then
# runs upstream's code: the behavior must be the same as upstream's.
# ---------------------------------------------------------------------------


def _modules():
    cloudpickle = _upstream_cloudpickle()
    return [("upstream", cloudpickle), ("rust", cloudpickle_rust)]


def _workload(cp, extra=False):
    """Pickle (and unpickle) all the parity objects with module cp."""
    import gc
    import weakref

    # The objects of previous runs must not be in the caches (weak references,
    # but the code objects of the nested functions of _parity_objects live
    # as long as it).
    gc.collect()
    cp.cloudpickle._extract_code_globals_cache.clear()
    payloads = []
    objects = _parity_objects()
    if extra:
        objects.append(weakref.WeakSet([abc.ABC]))
    for obj in objects:
        payload = cp.dumps(obj, protocol=5)
        pickle.loads(payload)
        payloads.append(_normalized_opcodes(payload))
    if extra:
        deep = []
        for _ in range(100_000):
            deep = [deep]
        with pytest.raises(pickle.PicklingError):
            cp.dumps(deep)
    return payloads


_INTERNAL_FUNCTIONS = [
    "_getattribute",
    "_pickle_getattribute",
    "_whichmodule",
    "_lookup_module_and_qualname",
    "_is_registered_pickle_by_value",
    "_should_pickle_by_reference",
    "_get_or_create_tracker_id",
    "_lookup_class_or_track",
    "_walk_global_ops",
    "_extract_code_globals",
    "_find_imported_submodules",
    "_get_cell_contents",
    "_extract_class_dict",
    "_class_getstate",
    "_get_bases",
    "_enum_getnewargs",
    "_enum_getstate",
    "_class_getnewargs",
    "_dynamic_class_reduce",
    "_class_reduce",
    "_decompose_typevar",
    "_function_getstate",
]


@pytest.mark.parametrize("name", _INTERNAL_FUNCTIONS)
def test_replaced_functions_are_called_like_upstream(name, monkeypatch):
    results = {}
    for label, cp in _modules():
        _workload(cp)  # warm-up: lazy imports...
        calls = []
        original = getattr(cp.cloudpickle, name)

        def spy(*args, __original=original, **kwargs):
            calls.append(len(args) + len(kwargs))
            return __original(*args, **kwargs)

        with monkeypatch.context() as m:
            m.setattr(cp.cloudpickle, name, spy)
            payloads = _workload(cp)
        results[label] = (calls, payloads)
    assert results["upstream"][0], f"{name} was not called"
    assert results["rust"] == results["upstream"]


class _RecordingModule:
    """Stand-in for a module, recording the accesses to its attributes."""

    def __init__(self, module, log):
        object.__setattr__(self, "_recorded", (module, log))

    def __getattribute__(self, name):
        module, log = object.__getattribute__(self, "_recorded")
        log.append(name)
        return getattr(module, name)


_IMPORTED_MODULES = [
    "types",
    "sys",
    "pickle",
    "io",
    "itertools",
    "uuid",
    "logging",
    "weakref",
    "typing",
    "abc",
    "dis",
    "builtins",
]


@pytest.mark.parametrize("name", _IMPORTED_MODULES)
def test_replaced_modules_are_used_like_upstream(name, monkeypatch):
    results = {}
    for label, cp in _modules():
        _workload(cp, extra=True)
        log = []
        with monkeypatch.context() as m:
            m.setattr(
                cp.cloudpickle,
                name,
                _RecordingModule(getattr(cp.cloudpickle, name), log),
            )
            payloads = _workload(cp, extra=True)
            cp.cloudpickle.is_tornado_coroutine(len) if name == "sys" else None
        results[label] = (log, payloads)
    assert results["upstream"][0], f"{name} was not used"
    assert results["rust"] == results["upstream"]


def _outcome(func):
    """The result of func(), or the type and message of the exception."""
    try:
        return func()
    except Exception as e:
        return (type(e).__name__, str(e).replace("cloudpickle_rust.", "cloudpickle."))


def _same_outcome(func):
    outcomes = [_outcome(lambda: func(cp)) for _, cp in _modules()]
    assert outcomes[0] == outcomes[1]
    return outcomes[0]


def _store_and_load_globals():
    global GLOBAL_VARIABLE
    GLOBAL_VARIABLE = os


def test_replaced_state_is_used_like_upstream(monkeypatch):
    import json
    import weakref

    def registry(cp):
        with monkeypatch.context() as m:
            m.setattr(cp.cloudpickle, "_PICKLE_BY_VALUE_MODULES", {"json"})
            by_reference = cp.cloudpickle._should_pickle_by_reference(json.dumps)
            cp.register_pickle_by_value(collections)
            registered = cp.list_registry_pickle_by_value()
        return (
            by_reference,
            sorted(registered),
            sorted(cp.list_registry_pickle_by_value()),
        )

    assert _same_outcome(registry) == (False, ["collections", "json"], [])

    def trackers(cp):
        class C:
            pass

        by_class, by_id = weakref.WeakKeyDictionary(), weakref.WeakValueDictionary()
        with monkeypatch.context() as m:
            m.setattr(cp.cloudpickle, "_DYNAMIC_CLASS_TRACKER_BY_CLASS", by_class)
            m.setattr(cp.cloudpickle, "_DYNAMIC_CLASS_TRACKER_BY_ID", by_id)
            tracker_id = cp.cloudpickle._get_or_create_tracker_id(C)
            tracked = cp.cloudpickle._lookup_class_or_track(tracker_id, None)
        return by_class[C] == tracker_id, by_id[tracker_id] is C, tracked is C

    assert _same_outcome(trackers) == (True, True, True)

    def code_cache(cp):
        cache = weakref.WeakKeyDictionary()

        def f():
            return GLOBAL_VARIABLE

        with monkeypatch.context() as m:
            m.setattr(cp.cloudpickle, "_extract_code_globals_cache", cache)
            names = cp.cloudpickle._extract_code_globals(f.__code__)
        return list(names), f.__code__ in cache

    assert _same_outcome(code_cache) == (["GLOBAL_VARIABLE"], True)

    def constants(cp):
        mod = cp.cloudpickle
        with monkeypatch.context() as m:
            m.setattr(mod, "_BUILTIN_TYPE_NAMES", {int: "int"})
            m.setattr(mod, "GLOBAL_OPS", (mod.LOAD_GLOBAL,))
            m.setattr(mod, "_DATACLASSE_FIELD_TYPE_SENTINELS", {"x": 1})
            sentinel = mod._get_dataclass_field_type_sentinel("x")
            ops = list(mod._walk_global_ops(_store_and_load_globals.__code__))
            reducer, args = mod._class_reduce(int)
        return sentinel, ops, reducer is mod._builtin_type, args

    assert _same_outcome(constants) == (1, ["os"], True, ("int",))

    def empty_cell_value(cp):
        mod = cp.cloudpickle
        with monkeypatch.context() as m:
            m.setattr(mod, "_empty_cell_value", object())
            # The default value of _make_cell is the original sentinel.
            cell = mod._make_cell()
            contents = mod._get_cell_contents(mod._make_empty_cell())
        return (
            cell.cell_contents is mod._empty_cell_value,
            contents is mod._empty_cell_value,
        )

    assert _same_outcome(empty_cell_value) == (True, False)

    def builtins_namespace(cp):
        namespace = {"__builtins__": {}}
        with monkeypatch.context() as m:
            m.setattr(cp.cloudpickle, "__builtins__", namespace)
            f = cp.cloudpickle._make_function(
                (lambda: None).__code__, {}, "f", None, None
            )
        return f.__globals__["__builtins__"] is namespace

    assert _same_outcome(builtins_namespace) is True


def test_replaced_stdlib_attributes_are_used_like_upstream(monkeypatch):
    import uuid
    import warnings

    def tracker_id(cp):
        class C:
            pass

        with monkeypatch.context() as m:
            m.setattr(uuid, "uuid4", lambda: uuid.UUID(int=7))
            return cp.cloudpickle._get_or_create_tracker_id(C)

    assert _same_outcome(tracker_id) == uuid.UUID(int=7).hex

    def tornado_warning(cp):
        calls = []
        with monkeypatch.context() as m:
            m.setattr(warnings, "warn", lambda *args, **kwargs: calls.append(kwargs))
            cp.cloudpickle.is_tornado_coroutine(len)
        return calls

    assert _same_outcome(tornado_warning) == [{"category": DeprecationWarning}]


def test_empty_cell_value_is_an_instance_of_a_python_class():
    cloudpickle = _upstream_cloudpickle()
    import weakref

    for cp in (cloudpickle, cloudpickle_rust):
        sentinel = cp.cloudpickle._empty_cell_value
        cls = type(sentinel)
        assert type(cls) is type
        assert cls.__name__ == cls.__qualname__ == "_empty_cell_value"
        assert cls.__doc__ == "Sentinel for empty closures."
        assert sentinel.__reduce__() == "_empty_cell_value"
        assert pickle.loads(pickle.dumps(sentinel)) is sentinel
        weakref.ref(sentinel)
        sentinel.attribute = 1
        del sentinel.attribute
    assert type(cloudpickle_rust.cloudpickle._empty_cell_value).__module__ == (
        "cloudpickle_rust.cloudpickle"
    )
    assert set(vars(type(cloudpickle.cloudpickle._empty_cell_value))) == set(
        vars(type(cloudpickle_rust.cloudpickle._empty_cell_value))
    )


def test_error_messages_are_upstream_ones():
    def unpacking(cp):
        mod = cp.cloudpickle

        class C:
            pass

        return [
            _outcome(lambda: mod._class_setstate(C, state))
            for state in [(1, 2, 3), [1], (), 1, "ab", {1: 2, 3: 4, 5: 6}]
        ] + [_outcome(lambda: mod._function_setstate(lambda: 1, (1, 2, 3)))]

    _same_outcome(unpacking)
    _same_outcome(lambda cp: cp.cloudpickle._getattribute(os, "path.missing"))
    _same_outcome(lambda cp: cp.cloudpickle._getattribute(os, "<locals>.x"))

    class FakeFile:
        closed = False

        def __init__(self, mode, name):
            self.mode, self.name = mode, name

        def tell(self):
            raise OSError("tell")

    for mode, name in [(("w",), "f"), (("w", "x"), "f"), ("r", ("a", "b")), ("r", "f")]:
        _same_outcome(lambda cp: cp.cloudpickle._file_reduce(FakeFile(mode, name)))

    class Module(types.ModuleType):
        def __format__(self, spec):
            return "<formatted module>"

    _same_outcome(lambda cp: cp.register_pickle_by_value(Module("unknown_module")))
    _same_outcome(lambda cp: cp.unregister_pickle_by_value(Module("unknown_module")))


def test_code_functions_handle_other_objects_like_upstream():
    def f():
        return os.path, GLOBAL_VARIABLE

    def walk(cp):
        ops = cp.cloudpickle._walk_global_ops(f)
        return list(ops), _outcome(lambda: list(cp.cloudpickle._walk_global_ops(1)))

    assert _same_outcome(walk)[0] == ["os", "GLOBAL_VARIABLE"]
    _same_outcome(lambda cp: cp.cloudpickle._extract_code_globals(f))

    package = types.ModuleType("_cloudpickle_rust_test_package")
    package.__package__ = "_cloudpickle_rust_test_package"
    _same_outcome(lambda cp: cp.cloudpickle._find_imported_submodules(1, [package]))


def test_functions_match_upstream():
    cloudpickle = _upstream_cloudpickle()
    import inspect

    def signature(func):
        text = str(inspect.signature(func))
        return re.sub(r" at 0x[0-9a-fA-F]+", "", text).replace(
            "cloudpickle_rust.", "cloudpickle."
        )

    upstream, rust = cloudpickle.cloudpickle, cloudpickle_rust.cloudpickle
    pairs = [
        (name, func, getattr(rust, name))
        for name, func in vars(upstream).items()
        if isinstance(func, types.FunctionType) and func.__module__ == upstream.__name__
    ]
    pairs += [
        (f"Pickler.{name}", func, vars(rust.Pickler)[name])
        for name, func in vars(upstream.Pickler).items()
        if isinstance(func, types.FunctionType) and name != "reducer_override"
    ]
    assert len(pairs) > 60
    for name, expected, func in pairs:
        assert isinstance(func, types.FunctionType), name
        assert signature(func) == signature(expected), name
        assert func.__doc__ == expected.__doc__, name
        assert func.__qualname__ == expected.__qualname__, name
        assert func.__module__ == rust.__name__, name
        assert func.__globals__ is vars(rust), name
    # The only method which is not a Python function (the C pickler calls it for
    # every object).
    reducer_override = vars(rust.Pickler)["reducer_override"]
    assert reducer_override.__doc__ == upstream.Pickler.reducer_override.__doc__


def test_bundled_upstream_source_is_upstream():
    cloudpickle = _upstream_cloudpickle()
    bundled = os.path.join(
        os.path.dirname(__file__), os.pardir, "src", "upstream", "cloudpickle.py"
    )
    if not os.path.exists(bundled):
        pytest.skip("not run from a source tree")
    with open(bundled, encoding="utf-8") as f, open(
        cloudpickle.cloudpickle.__file__, encoding="utf-8"
    ) as g:
        assert f.read() == g.read()
