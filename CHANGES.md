0.3.0
=====

- Add `cloudpickle_rust.cloudpickle.DYNAMIC_CODE_FILENAME`, the `co_filename`
  given to the code pickled by value (`"<dynamic-code>"` by default, as
  upstream). Setting it to `None` keeps the original filenames, as cloudpickle
  3.1 and earlier did, for the tracebacks and the profilers of the processes
  that load the code, for instance the Python UDFs of Apache Spark.

- Like upstream's, the functions of `cloudpickle_rust.cloudpickle` and the
  methods of `Pickler` (except `reducer_override`) are now Python functions,
  with upstream's signatures (including `_make_cell(value=_empty_cell_value)`),
  docstrings and globals, which call their Rust implementation: they are
  `types.FunctionType` instances, so that `Pickler` subclasses see the same
  calls to `_function_reduce` as with upstream cloudpickle, and warnings
  raised while pickling are attributed to the module like upstream's.

- Replacing a global of `cloudpickle_rust.cloudpickle` (its state such as
  `_PICKLE_BY_VALUE_MODULES`, its functions, the modules it imported) or an
  attribute of a standard library module used by cloudpickle (such as
  `types.FunctionType` or `uuid.uuid4`), for instance with `monkeypatch`, now
  changes the behavior like with upstream cloudpickle: the affected functions
  run upstream's code, from a copy of upstream's `cloudpickle.py` included in
  the package.

- `_extract_code_globals_cache` is a `weakref.WeakKeyDictionary` and
  `_empty_cell_value` is an instance of a Python class, like upstream's.

- Same error messages as upstream cloudpickle for `_getattribute` on Python
  3.12 and 3.13, for the unpacking of the states of `_function_setstate` and
  `_class_setstate` on Python 3.14, for file objects whose `mode` or `name` is
  not a string, and when `register_pickle_by_value` and
  `unregister_pickle_by_value` format a module.

- `_walk_global_ops`, `_extract_code_globals` and `_find_imported_submodules`
  accept the same objects as upstream's (for instance a function rather than a
  code object).

- On PyPy, the functions of the module are saved by `Pickler.save_function`
  like upstream's, and `Pickler` has no `reducer_override` method anymore.

- Pickling functions and classes is about 10% slower than with 0.1.0 (the
  cost of the checks of the globals and of upstream's
  `weakref.WeakKeyDictionary` cache): see the updated benchmark in README.md.

- Test coverage: the `coverage` job of the Automated Tests workflow measures
  the coverage of the Rust code (with cargo-llvm-cov) and of the Python code
  with every supported version of Python and PyPy, and uploads it to Codecov.

0.1.0
=====

- Initial release: a Rust implementation (built with PyO3 and maturin) of
  [cloudpickle](https://github.com/cloudpipe/cloudpickle) 3.2.0.dev0
  (upstream commit `4af5936`, "MNT: account for removed opcode in Python 3.16
  (#599)"), including the deterministic `co_filename` of dynamic code
  ([PR#560](https://github.com/cloudpipe/cloudpickle/pull/560)).

- Same public and private API as `cloudpickle`, under the `cloudpickle_rust`
  package: `cloudpickle_rust.cloudpickle` is the Rust extension module
  equivalent to `cloudpickle/cloudpickle.py`.

- Payloads are identical to the ones of upstream cloudpickle (same pickle
  opcodes and memoization), except for the module of the reconstructors
  (`cloudpickle_rust.cloudpickle` instead of `cloudpickle.cloudpickle`).

- The upstream test-suite passes with exactly the same outcomes as upstream
  cloudpickle on CPython 3.8 to 3.14 and free-threaded CPython 3.13t and
  3.14t.

- 1.4x to 1.7x faster than cloudpickle to pickle dynamic functions, classes
  and their instances (see `benchmarks/bench.py`).

- Supports PyPy 3.11 (PyPy 7.3.20+ and 8.0), on which upstream cloudpickle
  3.1.2 / 3.2.0.dev0 cannot be used: it fails at import time because
  `pickle.Pickler` became a native class without a `dispatch` attribute (and
  its code objects reducer does not match the signature of `types.CodeType`
  in PyPy 7.3, which differs from the one of CPython and PyPy 8).

- Supports free-threaded CPython builds without re-enabling the GIL.

- Licensed under the Apache License, Version 2.0. The code derived from
  cloudpickle and the files copied from it keep their BSD 3-Clause license
  notice (see `licenses/LICENSE-cloudpickle.txt` and `NOTICE`).
