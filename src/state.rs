//! Process-wide state of the extension module.
//!
//! Upstream `cloudpickle.py` keeps its state in module-level globals
//! (`_PICKLE_BY_VALUE_MODULES`, `_DYNAMIC_CLASS_TRACKER_BY_CLASS`, ...). The
//! very same Python objects are created here, exposed as attributes of the
//! `cloudpickle_rust.cloudpickle` module, and also cached in [`Globals`] so
//! that the Rust code can access them (and the stdlib objects it needs)
//! without any attribute lookup.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyModule, PySet, PyType};

/// Opcodes of the instructions referencing global variables.
pub(crate) struct Opcodes {
    pub store_global: u8,
    /// `None` on Python versions where the opcode was removed (3.16+).
    pub delete_global: Option<u8>,
    pub load_global: u8,
    pub extended_arg: u8,
    /// How the oparg of `LOAD_GLOBAL` encodes the index of the name: on
    /// CPython 3.11+ it is `namei << 1 | push_null` (shift of 1), on older
    /// CPython versions and on PyPy it is `namei` (shift of 0). Measured at
    /// import time on a probe function; `None` if the encoding is unknown, in
    /// which case `dis.get_instructions` is used like in upstream.
    pub load_global_shift: Option<u32>,
}

pub(crate) struct Globals {
    /// `(major, minor)` of the running interpreter.
    pub version: (u8, u8),

    // Our own module and its namespace.
    pub module: Py<PyModule>,
    pub module_dict: Py<PyDict>,
    /// The type of the functions of this module (`builtin_function_or_method`).
    pub builtin_function_type: Py<PyAny>,

    /// Running on PyPy (`PYPY` in upstream).
    pub pypy: bool,

    // Standard library modules. Upstream accesses functions of these modules
    // (`uuid.uuid4`, `io.BytesIO`, `logging.getLogger`, ...) as attributes at
    // call time, so they are looked up when needed rather than cached: this
    // keeps the behavior identical when they are mocked or monkeypatched.
    pub sys: Py<PyModule>,
    pub builtins: Py<PyModule>,
    pub builtins_dict: Py<PyDict>,
    pub types_module: Py<PyModule>,
    pub abc_module: Py<PyModule>,
    pub io_module: Py<PyModule>,
    pub logging_module: Py<PyModule>,
    pub typing_module: Py<PyModule>,
    pub uuid_module: Py<PyModule>,
    pub weakref_module: Py<PyModule>,
    pub pickle_module: Py<PyModule>,

    // Types (and objects bound at import time by upstream, like the names
    // imported with `from collections import OrderedDict`).
    pub function_type: Py<PyAny>,
    pub code_type: Py<PyAny>,
    pub cell_type: Py<PyAny>,
    pub module_type: Py<PyAny>,
    pub property_type: Py<PyAny>,
    pub str_type: Py<PyAny>,
    pub bytes_type: Py<PyAny>,
    pub none_type: Py<PyAny>,
    pub ellipsis_type: Py<PyAny>,
    pub notimplemented_type: Py<PyAny>,
    pub enum_type: Py<PyAny>,
    pub ordered_dict: Py<PyAny>,
    pub getattr: Py<PyAny>,
    pub id: Py<PyAny>,
    pub pickling_error: Py<PyType>,
    /// `type(float.__new__.__code__)` on PyPy, None on CPython.
    pub builtin_code_type: Py<PyAny>,

    // Module-level state (also exposed as attributes of the module).
    pub pickle_by_value_modules: Py<PySet>,
    pub tracker_by_class: Py<PyAny>,
    pub tracker_by_id: Py<PyAny>,
    pub tracker_lock: Py<PyAny>,
    pub code_globals_cache: Py<PyDict>,
    pub builtin_type_names: Py<PyDict>,
    pub dataclass_sentinels: Py<PyDict>,
    pub empty_cell_value: Py<PyAny>,

    // Functions of this module referenced by the reduce tuples. They must be
    // the very objects stored in the module namespace: they are pickled by
    // reference (module + qualified name) and pickle checks their identity.
    pub make_function: Py<PyAny>,
    pub function_setstate: Py<PyAny>,
    pub make_skeleton_class: Py<PyAny>,
    pub make_skeleton_enum: Py<PyAny>,
    pub class_setstate: Py<PyAny>,
    pub make_typevar: Py<PyAny>,
    pub builtin_type: Py<PyAny>,
    pub make_empty_cell: Py<PyAny>,
    pub make_cell: Py<PyAny>,
    pub subimport: Py<PyAny>,
    pub dynamic_subimport: Py<PyAny>,
    pub file_reconstructor: Py<PyAny>,
    pub make_dict_keys: Py<PyAny>,
    pub make_dict_values: Py<PyAny>,
    pub make_dict_items: Py<PyAny>,
    pub get_dataclass_field_type_sentinel: Py<PyAny>,

    // Classes created at module initialization.
    pub pickler_type: Py<PyType>,
    pub dispatch_table_type: Py<PyType>,

    pub opcodes: Opcodes,
}

pub(crate) static GLOBALS: PyOnceLock<Globals> = PyOnceLock::new();

/// Access the module state. Only Rust code reachable from Python objects
/// created by the module initialization calls this, hence after `GLOBALS`
/// has been set.
#[inline]
pub(crate) fn globals(py: Python<'_>) -> &'static Globals {
    GLOBALS
        .get(py)
        .expect("cloudpickle_rust.cloudpickle is not initialized")
}

impl Globals {
    #[inline]
    pub fn at_least(&self, major: u8, minor: u8) -> bool {
        self.version >= (major, minor)
    }
}
