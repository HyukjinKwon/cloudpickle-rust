# cloudpickle-rust

[![Automated Tests](https://github.com/HyukjinKwon/cloudpickle-rust/actions/workflows/testing.yml/badge.svg?branch=main&event=push)](https://github.com/HyukjinKwon/cloudpickle-rust/actions/workflows/testing.yml)
[![codecov.io](https://codecov.io/github/HyukjinKwon/cloudpickle-rust/coverage.svg?branch=main)](https://codecov.io/github/HyukjinKwon/cloudpickle-rust?branch=main)
[![PyPI](https://img.shields.io/pypi/v/cloudpickle-rust.svg)](https://pypi.org/project/cloudpickle-rust/)

`cloudpickle-rust` is a Rust implementation of
[cloudpickle](https://github.com/cloudpipe/cloudpickle), the library that makes
it possible to serialize Python constructs not supported by the default
`pickle` module from the Python standard library.

It is a complete rewrite of `cloudpickle/cloudpickle.py` in Rust (with
[PyO3](https://pyo3.rs)), exposing exactly the same API, producing the same
payloads, and passing the upstream test-suite with exactly the same results
(see [Compatibility](#compatibility-with-cloudpickle)).

`cloudpickle` is especially useful for **cluster computing** where Python
code is shipped over the network to execute on remote hosts, possibly close
to the data.

Among other things, `cloudpickle` supports pickling for **lambda functions**
along with **functions and classes defined interactively** in the
`__main__` module (for instance in a script, a shell or a Jupyter notebook).

Cloudpickle can only be used to send objects between the **exact same version
of Python**.

Using `cloudpickle` for **long-term object storage is not supported and
strongly discouraged.**

**Security notice**: one should **only load pickle data from trusted sources** as
otherwise `pickle.load` can lead to arbitrary code execution resulting in a critical
security vulnerability.


Installation
------------

The latest release of `cloudpickle-rust` is available from
[pypi](https://pypi.org/project/cloudpickle-rust/):

    pip install cloudpickle-rust

Binary wheels are provided for:

| Platform | CPython 3.8 – 3.14 | Free-threaded CPython 3.14t | PyPy 3.11 (7.3, 8.0) |
|---|---|---|---|
| Linux, manylinux (x86_64, aarch64) | ✓ | ✓ | ✓ (PyPy 8.0: manylinux_2_28) |
| Linux, manylinux (x86) | 3.9 – 3.14 | ✓ | |
| Linux, manylinux (armv7, s390x, ppc64le) | ✓ | ✓ | |
| Linux, musllinux (x86_64, x86, aarch64, armv7) | ✓ | ✓ | |
| macOS (x86_64, arm64) | ✓ | ✓ | ✓ |
| Windows (x64) | ✓ | ✓ | |
| Windows (x86) | ✓ | | |
| Windows (arm64) | 3.11 – 3.14 | ✓ | |

On other platforms (and on the free-threaded CPython 3.13t, for which maturin
does not build wheels since free-threading is experimental in 3.13),
installing from the source distribution requires a Rust toolchain.

The package is importable as `cloudpickle_rust`, so that it can be installed
alongside `cloudpickle`. To use it as a drop-in replacement:

```python
import cloudpickle_rust as cloudpickle
```

Like `cloudpickle`, the payloads reference the functions used to reconstruct
dynamic functions and classes (as `cloudpickle_rust.cloudpickle.<name>`), so
`cloudpickle-rust` must also be installed in the Python environment that loads
them (with plain `pickle.loads`).


Examples
--------

Pickling a lambda expression:

```python
>>> import cloudpickle_rust as cloudpickle
>>> squared = lambda x: x ** 2
>>> pickled_lambda = cloudpickle.dumps(squared)

>>> import pickle
>>> new_squared = pickle.loads(pickled_lambda)
>>> new_squared(2)
4
```

Pickling a function interactively defined in a Python shell session
(in the `__main__` module):

```python
>>> CONSTANT = 42
>>> def my_function(data: int) -> int:
...     return data + CONSTANT
...
>>> pickled_function = cloudpickle.dumps(my_function)
>>> depickled_function = pickle.loads(pickled_function)
>>> depickled_function
<function __main__.my_function(data:int) -> int>
>>> depickled_function(43)
85
```


Overriding pickle's serialization mechanism for importable constructs:
----------------------------------------------------------------------

An important difference between `cloudpickle` and `pickle` is that
`cloudpickle` can serialize a function or class **by value**, whereas `pickle`
can only serialize it **by reference**. Serialization by reference treats
functions and classes as attributes of modules, and pickles them through
instructions that trigger the import of their module at load time.
Serialization by reference is thus limited in that it assumes that the module
containing the function or class is available/importable in the unpickling
environment. This assumption breaks when pickling constructs defined in an
interactive session, a case that is automatically detected by `cloudpickle`,
that pickles such constructs **by value**.

Another case where the importability assumption is expected to break is when
developing a module in a distributed execution environment: the worker
processes may not have access to the said module, for example if they live on a
different machine than the process in which the module is being developed. By
itself, `cloudpickle` cannot detect such "locally importable" modules and
switch to serialization by value; instead, it relies on its default mode, which
is serialization by reference. However, since `cloudpickle 2.0.0`, one can
explicitly specify modules for which serialization by value should be used,
using the
`register_pickle_by_value(module)`/`/unregister_pickle_by_value(module)` API:

```python
>>> import cloudpickle_rust as cloudpickle
>>> import my_module
>>> cloudpickle.register_pickle_by_value(my_module)
>>> cloudpickle.dumps(my_module.my_function)  # my_function is pickled by value
>>> cloudpickle.unregister_pickle_by_value(my_module)
>>> cloudpickle.dumps(my_module.my_function)  # my_function is pickled by reference
```

Using this API, there is no need to re-install the new version of the module on
all the worker nodes nor to restart the workers: restarting the client Python
process with the new source code is enough.

Note that this feature is still **experimental**, and may fail in the following
situations:

- If the body of a function/class pickled by value contains an `import` statement:
  ```python
  >>> def f():
  >>> ... from another_module import g
  >>> ... # calling f in the unpickling environment may fail if another_module
  >>> ... # is unavailable
  >>> ... return g() + 1
  ```

- If a function pickled by reference uses a function pickled by value during its execution.


Compatibility with cloudpickle
------------------------------

`cloudpickle-rust` 0.3.0 implements cloudpickle 3.2.0.dev0 (upstream commit
[`4af5936`](https://github.com/cloudpipe/cloudpickle/commit/4af5936c328de0b33fa9c636e210d9a9e1e97dd3)).

- **Same structure and API.** The package mirrors upstream: `cloudpickle_rust/__init__.py`,
  `cloudpickle_rust/cloudpickle_fast.py` and `cloudpickle_rust.cloudpickle`,
  the Rust extension module equivalent to `cloudpickle/cloudpickle.py`. Every
  function, class and module-level attribute of `cloudpickle.cloudpickle`
  exists in `cloudpickle_rust.cloudpickle` (`Pickler`, `dumps`,
  `register_pickle_by_value`, but also the private helpers such as
  `_should_pickle_by_reference` or `_make_skeleton_class`).
- **Same functions.** Like upstream's, the functions of
  `cloudpickle_rust.cloudpickle` and the methods of its `Pickler` (except
  `reducer_override`, see below) are Python functions, with the signatures,
  the docstrings and the globals (the namespace of the module) of upstream's:
  they call their Rust implementation, which they keep in their closure.
- **Same globals.** Like upstream's code, cloudpickle-rust reads the globals of
  its module (its state such as `_PICKLE_BY_VALUE_MODULES`, its functions, the
  modules it imported) and the attributes of the standard library modules it
  uses (`types.FunctionType`, `uuid.uuid4`, `sys.modules`...) when it runs:
  replacing any of them, for instance with `monkeypatch`, changes its behavior
  like upstream's. Each Rust function checks that the globals read by the
  upstream function still hold their initial values (a dictionary lookup
  each); if not, it runs the upstream function itself, from a copy of
  upstream's `cloudpickle.py` included in the package, with the namespace of
  `cloudpickle_rust.cloudpickle` as its globals.
- **Same payloads.** For a given object, the pickle opcodes (including the
  memoization of objects, which cloudpickle carefully controls to produce
  deterministic payloads) are identical to the ones produced by upstream
  cloudpickle, except for the module of the reconstructors.
- **Same test results.** The test-suite of this repository is the upstream
  test-suite, where only `import cloudpickle` was replaced by
  `import cloudpickle_rust as cloudpickle`. On CPython 3.8 to 3.14 and
  free-threaded 3.13t and 3.14t, every test has the same outcome (pass, skip
  or expected failure) as with upstream cloudpickle. Additional tests compare
  the payloads, the functions and the behavior when globals are replaced with
  upstream cloudpickle's.
- **PyPy 3.11.** Upstream cloudpickle cannot be imported on PyPy 3.11 (PyPy
  7.3.20+ and 8.0), whose `pickle.Pickler` became a native class without the
  `dispatch` attribute that its PyPy implementation relies on (and its reducer
  of code objects does not match the signature of `types.CodeType` of PyPy
  7.3). `cloudpickle-rust` uses the pure-Python pickler on PyPy, like upstream
  did with earlier PyPy versions, detects the signature of `types.CodeType`,
  and passes the whole test-suite on PyPy 7.3.23 and 8.0.1. On PyPy 8.0.1,
  with upstream's import fixed, every upstream test has the same outcome.

The remaining implementation-level differences, which do not change what is
pickled:

- `Pickler.reducer_override`, which the C pickler calls for every object, is
  a method descriptor implemented in Rust (like the methods of builtin types)
  rather than a Python function: its `self` parameter is positional-only.
- `Pickler.dispatch_table` is a subclass of `collections.ChainMap`
  (`_DispatchTable`) with a native `__getitem__`.
- `_walk_global_ops` decodes the bytecode of code objects directly (the
  encoding of the global-referencing instructions is measured when the module
  is imported) rather than with `dis.get_instructions`, and returns an
  iterator rather than a generator.
- The builtins (`isinstance`, `getattr`, ...) are not part of the globals
  checked by the Rust functions: shadowing them with globals of
  `cloudpickle_rust.cloudpickle`, or replacing them in the `builtins` module,
  does not change its behavior (unless it runs upstream's code because another
  global was replaced).
- The Rust functions have no frames: exceptions raised by them have shorter
  tracebacks, `dumps` passes `None` rather than a traceback to the `__exit__`
  method of its `io.BytesIO` for such exceptions, and deeply nested objects
  can reach the recursion limit at a slightly different depth.
- The docstring of the module also states the license of cloudpickle-rust
  (unless docstrings are removed, with `python -OO`).
- Like other PyO3 extension modules
  ([PyO3/pyo3#576](https://github.com/PyO3/pyo3/issues/576)),
  `cloudpickle_rust` cannot be imported in subinterpreters (for instance the
  ones of `concurrent.interpreters`, new in Python 3.14): it raises
  `ImportError` there, while upstream cloudpickle can be used in any
  interpreter.

One extension, which upstream does not have:

- `cloudpickle_rust.cloudpickle.DYNAMIC_CODE_FILENAME` is the `co_filename`
  given to the code of the functions and classes pickled by value:
  `"<dynamic-code>"` by default, as upstream since
  [PR#560](https://github.com/cloudpipe/cloudpickle/pull/560), so that the
  payloads do not depend on where the code is defined. Set it to `None` to
  keep the original filenames, as cloudpickle 3.1 and earlier did, so that
  the tracebacks and the profilers of the processes that load the code show
  where it was defined. Like `DEFAULT_PROTOCOL`, it is looked up at call time:

  ```python
  >>> import cloudpickle_rust.cloudpickle
  >>> cloudpickle_rust.cloudpickle.DYNAMIC_CODE_FILENAME = None
  ```


Performance
-----------

`cloudpickle-rust` produces the same payloads as cloudpickle while running
its logic natively: `reducer_override`, the reducers and state getters of
functions and classes, the bytecode analysis of functions, and the lookups in
the pickler's `dispatch_table` (which cloudpickle performs in Python for every
object that is not handled by `reducer_override`).

Time of `dumps` with [`benchmarks/bench.py`](benchmarks/bench.py), on an
Apple M3 Max with CPython 3.13.14 (release build of cloudpickle-rust,
cloudpickle 3.2.0.dev0, best of 15 interleaved runs):

| Payload | cloudpickle | cloudpickle-rust | Speedup |
|---|---:|---:|---:|
| A lambda | 10.6 µs | 8.0 µs | 1.32× |
| A closure using globals | 16.6 µs | 11.9 µs | 1.40× |
| A dynamic class | 27.5 µs | 19.1 µs | 1.44× |
| A dynamic dataclass | 75.4 µs | 54.3 µs | 1.39× |
| A dynamic enum | 19.8 µs | 13.0 µs | 1.52× |
| A generic class (`typing.Generic`) | 32.8 µs | 22.6 µs | 1.45× |
| 1,000 distinct lambdas | 5.14 ms | 3.99 ms | 1.29× |
| 100,000 instances of a dynamic class | 93.4 ms | 60.8 ms | 1.54× |
| 100,000 instances of a dynamic dataclass | 99.2 ms | 66.7 ms | 1.49× |
| 100,000 builtin objects (dict of lists) | 19.4 ms | 19.3 ms | 1.00× |

For the collections of instances, plain `pickle.dumps` (which pickles the
classes by reference instead of by value) takes 41.2 ms and 45.9 ms: the time
cloudpickle spends on top of the standard library pickler is about 2.5× smaller
with cloudpickle-rust. Builtin types (`dict`, `list`, `str`, `int`...) are
handled by the standard library pickler without involving cloudpickle at all,
hence the same speed (plain `pickle.dumps` takes 19.1 ms).


Running the tests
-----------------

- With `tox`, to test run the tests for all the supported versions of
  Python and PyPy:

      pip install tox
      tox

  or alternatively for a specific environment:

      tox -e py312


- With `pytest` to only run the tests for your current version of
  Python:

      pip install -e .
      pip install -r dev-requirements.txt
      PYTHONPATH='.:tests' pytest

A Rust toolchain ([rustup](https://rustup.rs)) is required to build the
extension module. During development, `maturin develop` rebuilds it in place.


Test coverage
-------------

The `coverage` job of the [`testing.yml`](.github/workflows/testing.yml)
workflow measures the coverage of the test-suite with every supported version
of Python and PyPy (on Linux), as the Rust code has version-specific code
paths, and uploads the reports to
[Codecov](https://codecov.io/github/HyukjinKwon/cloudpickle-rust), which merges
them. Each job also writes its coverage in its summary and uploads its HTML
reports as an artifact (`coverage-report-<Python version>`).

- The coverage of the Rust code (the implementation) is measured with
  [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) (LLVM
  source-based code coverage) on an instrumented build of the extension
  module, in the pytest process and in the Python subprocesses started by the
  tests.
- The coverage of the Python code is measured with
  [coverage.py](https://coverage.readthedocs.io), in subprocesses as well (as
  in upstream cloudpickle).

To measure it locally:

    rustup component add llvm-tools-preview
    cargo install cargo-llvm-cov
    eval "$(cargo llvm-cov show-env --sh)"
    cargo llvm-cov clean --workspace
    MATURIN_PEP517_ARGS="--profile dev" pip install -e .
    pip install -r dev-requirements.txt
    python ci/install_coverage_subprocess_pth.py
    COVERAGE_PROCESS_START=$PWD/.coveragerc PYTHONPATH='.:tests' pytest
    cargo llvm-cov report --html  # target/llvm-cov/html/index.html
    coverage combine --append && coverage html  # htmlcov/index.html

`cargo llvm-cov show-env` sets the environment variables (in the current
shell) with which cargo builds an instrumented extension module, and with which
the instrumented extension module writes its profile data where
`cargo llvm-cov report` reads it. Run `pip install -e .` in a shell without
them to go back to a regular build.


Releasing to PyPI
-----------------

The [`publish_to_pypi.yml`](.github/workflows/publish_to_pypi.yml) GitHub
Actions workflow builds the wheels of all the supported platforms and Python
versions (with [maturin-action](https://github.com/PyO3/maturin-action)) and the
source distribution, and tests the wheels, on every push. When a tag is pushed,
it publishes them to PyPI with the PyPI API token stored in the `PYPI_TOKEN`
secret of the repository:

1. Bump the version in `Cargo.toml` and `python/cloudpickle_rust/__init__.py`,
   and add an entry to `CHANGES.md`.
2. Push a tag, for instance `git tag v0.1.0 && git push origin v0.1.0`.

To build the distributions locally instead:

    pip install maturin
    maturin build --release    # wheel for the current interpreter
    maturin sdist              # source distribution


History
-------

`cloudpickle` was initially developed by [picloud.com](http://web.archive.org/web/20140721022102/http://blog.picloud.com/2013/11/17/picloud-has-joined-dropbox/) and shipped as part of
the client SDK.

A copy of `cloudpickle.py` was included as part of PySpark, the Python
interface to [Apache Spark](https://spark.apache.org/). Davies Liu, Josh
Rosen, Thom Neale and other Apache Spark developers improved it significantly,
most notably to add support for PyPy and Python 3.

The aim of the `cloudpickle` project is to make that work available to a wider
audience outside of the Spark ecosystem and to make it easier to improve it
further notably with the help of a dedicated non-regression test suite.

`cloudpickle-rust` is a Rust implementation of `cloudpickle`, which retains
its test-suite.


License
-------

`cloudpickle-rust` is licensed under the [Apache License, Version 2.0](LICENSE).

It is derived from `cloudpickle`, which is distributed under the
[BSD 3-Clause License](licenses/LICENSE-cloudpickle.txt): the code derived from
it and the files copied from it (listed at the end of [LICENSE](LICENSE)) keep
its copyright and license notice (see also [NOTICE](NOTICE)).
