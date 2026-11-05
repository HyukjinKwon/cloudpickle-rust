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
