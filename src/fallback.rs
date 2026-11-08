//! Call-time semantics of the globals of `cloudpickle_rust.cloudpickle`.
//!
//! The functions of upstream's `cloudpickle/cloudpickle.py` read the globals
//! of their module (its state such as `_PICKLE_BY_VALUE_MODULES`, the other
//! functions of the module, the imported modules) and the attributes of the
//! standard library modules they use (`types.FunctionType`, `uuid.uuid4`, ...)
//! every time they run: replacing any of them, for instance with
//! `monkeypatch.setattr`, changes their behavior. The native functions of this
//! crate work with the objects created when the module was initialized.
//!
//! To behave the same, each native function first checks that the globals and
//! attributes read by the upstream function (its [`Deps`]) are still these
//! objects: one dictionary lookup each. If one of them was replaced (or
//! deleted), the call is forwarded to the upstream function itself, compiled
//! from a copy of upstream's `cloudpickle/cloudpickle.py`
//! (`src/upstream/cloudpickle.py`) with the namespace of
//! `cloudpickle_rust.cloudpickle` as its globals: it runs exactly the upstream
//! code, with the current values of the globals. A few functions also forward
//! the inputs that only upstream's code handles (for instance a function
//! instead of a code object for `_walk_global_ops`).
//!
//! The builtins (`isinstance`, `getattr`, ...) are not part of the checks: like
//! the rest of the native code, the checks assume that they are not shadowed
//! by globals of the module, nor replaced in the `builtins` module.

use pyo3::exceptions::PyRuntimeError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyString, PyTuple};

use crate::state::{globals, GLOBALS};

/// A global read by an upstream function.
#[derive(Clone, Copy)]
pub(crate) enum Dep {
    /// A global variable of the module.
    Global(&'static str),
    /// An attribute of a module which is a global variable of the module,
    /// e.g. `types.FunctionType`.
    Attr(&'static str, &'static str),
}

use Dep::{Attr as A, Global as G};

struct ResolvedAttr {
    /// The namespace of the original module.
    namespace: Py<PyDict>,
    name: Py<PyString>,
    original: Option<Py<PyAny>>,
}

struct Resolved {
    global: Py<PyString>,
    /// `None` if the global did not exist when the module was initialized.
    original: Option<Py<PyAny>>,
    attr: Option<ResolvedAttr>,
}

/// The globals read by an upstream function, and their values when the
/// module was initialized.
pub(crate) struct Deps {
    /// Name of the upstream function (`Pickler.<name>` for the methods).
    name: &'static str,
    deps: &'static [Dep],
    resolved: PyOnceLock<Vec<Resolved>>,
}

fn same(py: Python<'_>, current: Option<Bound<'_, PyAny>>, original: &Option<Py<PyAny>>) -> bool {
    match (current, original) {
        (Some(current), Some(original)) => current.is(original.bind(py)),
        (None, None) => true,
        _ => false,
    }
}

impl Deps {
    pub(crate) const fn new(name: &'static str, deps: &'static [Dep]) -> Self {
        Self {
            name,
            deps,
            resolved: PyOnceLock::new(),
        }
    }

    fn resolve(&self, py: Python<'_>, module_dict: &Bound<'_, PyDict>) -> PyResult<()> {
        let mut resolved = Vec::with_capacity(self.deps.len());
        for dep in self.deps {
            let (global, attr) = match dep {
                Dep::Global(name) => (*name, None),
                Dep::Attr(module, attr) => (*module, Some(*attr)),
            };
            let global = PyString::intern(py, global);
            let original = module_dict.get_item(&global)?;
            let attr = match attr {
                None => None,
                Some(attr) => {
                    let module = original.as_ref().ok_or_else(|| {
                        PyRuntimeError::new_err(format!("{global} is not a global"))
                    })?;
                    let namespace = module
                        .getattr(intern!(py, "__dict__"))?
                        .cast_into::<PyDict>()?;
                    let name = PyString::intern(py, attr);
                    let original = namespace.get_item(&name)?.map(Bound::unbind);
                    Some(ResolvedAttr {
                        namespace: namespace.unbind(),
                        name: name.unbind(),
                        original,
                    })
                }
            };
            resolved.push(Resolved {
                global: global.unbind(),
                original: original.map(Bound::unbind),
                attr,
            });
        }
        let _ = self.resolved.set(py, resolved);
        Ok(())
    }

    /// Whether the globals read by the upstream function still hold their
    /// values from the initialization of the module.
    #[inline]
    pub(crate) fn pristine(&self, py: Python<'_>) -> PyResult<bool> {
        let (Some(resolved), Some(g)) = (self.resolved.get(py), GLOBALS.get(py)) else {
            // The module is being initialized.
            return Ok(true);
        };
        let module_dict = g.module_dict.bind(py);
        for dep in resolved {
            if !same(
                py,
                module_dict.get_item(dep.global.bind(py))?,
                &dep.original,
            ) {
                return Ok(false);
            }
            if let Some(attr) = &dep.attr {
                let current = attr.namespace.bind(py).get_item(attr.name.bind(py))?;
                if !same(py, current, &attr.original) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// The upstream implementation of the function.
    pub(crate) fn upstream<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        upstream_function(py, self.name)
    }

    /// Call the upstream implementation of the function.
    pub(crate) fn call<'py, A>(
        &self,
        py: Python<'py>,
        args: A,
        kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>>
    where
        A: pyo3::call::PyCallArgs<'py>,
    {
        self.upstream(py)?.call(args, kwargs)
    }
}

/// Return the result of the upstream implementation of a function if one of
/// the globals it reads was replaced: `upstream_if_patched!(py, DEPS, (args,))`
/// (optionally followed by a dict of keyword arguments).
macro_rules! upstream_if_patched {
    ($py:expr, $deps:expr, $args:expr) => {
        if !$deps.pristine($py)? {
            return $deps.call($py, $args, None);
        }
    };
    ($py:expr, $deps:expr, $args:expr, $kwargs:expr) => {
        if !$deps.pristine($py)? {
            return $deps.call($py, $args, $kwargs);
        }
    };
}
pub(crate) use upstream_if_patched;

// ---------------------------------------------------------------------------
// The globals read by each upstream function
// ---------------------------------------------------------------------------
//
// From the bytecode of upstream's functions (their LOAD_GLOBAL instructions,
// and the LOAD_ATTR instructions on the imported modules). When the native
// code of a function also implements other upstream functions inline (rather
// than calling their native implementation, which checks its own globals),
// their globals are included as well.

const TRACKERS: [Dep; 3] = [
    G("_DYNAMIC_CLASS_TRACKER_LOCK"),
    G("_DYNAMIC_CLASS_TRACKER_BY_CLASS"),
    G("_DYNAMIC_CLASS_TRACKER_BY_ID"),
];

pub(crate) static GET_OR_CREATE_TRACKER_ID: Deps = Deps::new(
    "_get_or_create_tracker_id",
    &[TRACKERS[0], TRACKERS[1], TRACKERS[2], A("uuid", "uuid4")],
);
pub(crate) static LOOKUP_CLASS_OR_TRACK: Deps = Deps::new("_lookup_class_or_track", &TRACKERS);
pub(crate) static REGISTER_PICKLE_BY_VALUE: Deps = Deps::new(
    "register_pickle_by_value",
    &[
        A("types", "ModuleType"),
        G("sys"),
        G("_PICKLE_BY_VALUE_MODULES"),
    ],
);
pub(crate) static UNREGISTER_PICKLE_BY_VALUE: Deps = Deps::new(
    "unregister_pickle_by_value",
    &[A("types", "ModuleType"), G("_PICKLE_BY_VALUE_MODULES")],
);
pub(crate) static LIST_REGISTRY_PICKLE_BY_VALUE: Deps = Deps::new(
    "list_registry_pickle_by_value",
    &[G("_PICKLE_BY_VALUE_MODULES")],
);
pub(crate) static IS_REGISTERED_PICKLE_BY_VALUE: Deps = Deps::new(
    "_is_registered_pickle_by_value",
    &[G("_PICKLE_BY_VALUE_MODULES")],
);
pub(crate) static GETATTRIBUTE: Deps = Deps::new("_getattribute", &[G("_pickle_getattribute")]);
pub(crate) static WHICHMODULE: Deps = Deps::new(
    "_whichmodule",
    &[
        G("sys"),
        A("types", "ModuleType"),
        G("_getattribute"),
        // inline _getattribute
        G("_pickle_getattribute"),
    ],
);
pub(crate) static SHOULD_PICKLE_BY_REFERENCE: Deps = Deps::new(
    "_should_pickle_by_reference",
    &[
        A("types", "FunctionType"),
        G("_lookup_module_and_qualname"),
        G("_is_registered_pickle_by_value"),
        A("types", "ModuleType"),
        G("sys"),
    ],
);
pub(crate) static LOOKUP_MODULE_AND_QUALNAME: Deps = Deps::new(
    "_lookup_module_and_qualname",
    &[
        G("_whichmodule"),
        G("sys"),
        G("_getattribute"),
        // inline _getattribute
        G("_pickle_getattribute"),
    ],
);

pub(crate) static WALK_GLOBAL_OPS: Deps = Deps::new(
    "_walk_global_ops",
    &[A("dis", "get_instructions"), G("GLOBAL_OPS")],
);
pub(crate) static EXTRACT_CODE_GLOBALS: Deps = Deps::new(
    "_extract_code_globals",
    &[
        G("_extract_code_globals_cache"),
        G("_walk_global_ops"),
        A("types", "CodeType"),
        G("_extract_code_globals"),
    ],
);
pub(crate) static FIND_IMPORTED_SUBMODULES: Deps = Deps::new(
    "_find_imported_submodules",
    &[A("types", "ModuleType"), G("sys")],
);

pub(crate) static IS_TORNADO_COROUTINE: Deps =
    Deps::new("is_tornado_coroutine", &[A("warnings", "warn"), G("sys")]);
pub(crate) static SUBIMPORT: Deps = Deps::new("subimport", &[G("sys")]);
pub(crate) static DYNAMIC_SUBIMPORT: Deps = Deps::new(
    "dynamic_subimport",
    &[A("types", "ModuleType"), G("builtins")],
);
pub(crate) static BUILTIN_TYPE: Deps = Deps::new("_builtin_type", &[G("types")]);
pub(crate) static MAKE_FUNCTION: Deps = Deps::new(
    "_make_function",
    &[G("__builtins__"), A("types", "FunctionType")],
);
pub(crate) static MAKE_CELL: Deps = Deps::new(
    "_make_cell",
    &[G("_make_empty_cell"), G("_empty_cell_value")],
);
pub(crate) static MAKE_SKELETON_CLASS: Deps = Deps::new(
    "_make_skeleton_class",
    &[
        A("sys", "intern"),
        A("types", "new_class"),
        G("_lookup_class_or_track"),
    ],
);
pub(crate) static MAKE_SKELETON_ENUM: Deps =
    Deps::new("_make_skeleton_enum", &[G("_lookup_class_or_track")]);
pub(crate) static MAKE_TYPEVAR: Deps = Deps::new(
    "_make_typevar",
    &[A("typing", "TypeVar"), G("_lookup_class_or_track")],
);
pub(crate) static MAKE_DICT_KEYS: Deps = Deps::new("_make_dict_keys", &[G("OrderedDict")]);
pub(crate) static MAKE_DICT_VALUES: Deps = Deps::new("_make_dict_values", &[G("OrderedDict")]);
pub(crate) static MAKE_DICT_ITEMS: Deps = Deps::new("_make_dict_items", &[G("OrderedDict")]);
pub(crate) static FUNCTION_SETSTATE: Deps = Deps::new("_function_setstate", &[G("__builtins__")]);
pub(crate) static CLASS_SETSTATE: Deps = Deps::new("_class_setstate", &[A("sys", "version_info")]);
pub(crate) static GET_DATACLASS_FIELD_TYPE_SENTINEL: Deps = Deps::new(
    "_get_dataclass_field_type_sentinel",
    &[G("_DATACLASSE_FIELD_TYPE_SENTINELS")],
);

pub(crate) static CLASS_GETNEWARGS: Deps = Deps::new(
    "_class_getnewargs",
    &[G("_get_bases"), G("_get_or_create_tracker_id")],
);
pub(crate) static ENUM_GETNEWARGS: Deps =
    Deps::new("_enum_getnewargs", &[G("_get_or_create_tracker_id")]);
const CLASS_GETSTATE_DEPS: [Dep; 4] = [
    G("_extract_class_dict"),
    A("abc", "ABCMeta"),
    A("abc", "_get_dump"),
    A("sys", "version_info"),
];
pub(crate) static CLASS_GETSTATE: Deps = Deps::new("_class_getstate", &CLASS_GETSTATE_DEPS);
pub(crate) static ENUM_GETSTATE: Deps = Deps::new(
    "_enum_getstate",
    &[
        G("_class_getstate"),
        // inline _class_getstate
        CLASS_GETSTATE_DEPS[0],
        CLASS_GETSTATE_DEPS[1],
        CLASS_GETSTATE_DEPS[2],
        CLASS_GETSTATE_DEPS[3],
    ],
);
pub(crate) static DYNAMIC_CLASS_REDUCE: Deps = Deps::new(
    "_dynamic_class_reduce",
    &[
        G("Enum"),
        G("_make_skeleton_enum"),
        G("_enum_getnewargs"),
        G("_enum_getstate"),
        G("_class_setstate"),
        G("_make_skeleton_class"),
        G("_class_getnewargs"),
        G("_class_getstate"),
    ],
);
pub(crate) static CLASS_REDUCE: Deps = Deps::new(
    "_class_reduce",
    &[
        G("_BUILTIN_TYPE_NAMES"),
        G("_builtin_type"),
        G("_should_pickle_by_reference"),
        G("_dynamic_class_reduce"),
    ],
);
pub(crate) static GET_CELL_CONTENTS: Deps =
    Deps::new("_get_cell_contents", &[G("_empty_cell_value")]);
pub(crate) static FUNCTION_GETSTATE: Deps = Deps::new(
    "_function_getstate",
    &[
        G("_extract_code_globals"),
        G("_get_cell_contents"),
        G("_find_imported_submodules"),
        A("itertools", "chain"),
    ],
);
pub(crate) static CELL_REDUCE: Deps =
    Deps::new("_cell_reduce", &[G("_make_empty_cell"), G("_make_cell")]);
pub(crate) static FILE_REDUCE: Deps = Deps::new(
    "_file_reduce",
    &[
        A("pickle", "PicklingError"),
        G("sys"),
        G("_file_reconstructor"),
    ],
);
pub(crate) static MAPPINGPROXY_REDUCE: Deps =
    Deps::new("_mappingproxy_reduce", &[A("types", "MappingProxyType")]);
pub(crate) static MODULE_REDUCE: Deps = Deps::new(
    "_module_reduce",
    &[
        G("_should_pickle_by_reference"),
        G("subimport"),
        G("dynamic_subimport"),
    ],
);
pub(crate) static METHOD_REDUCE: Deps = Deps::new("_method_reduce", &[A("types", "MethodType")]);
pub(crate) static LOGGER_REDUCE: Deps = Deps::new("_logger_reduce", &[A("logging", "getLogger")]);
pub(crate) static ROOT_LOGGER_REDUCE: Deps =
    Deps::new("_root_logger_reduce", &[A("logging", "getLogger")]);
pub(crate) static WEAKSET_REDUCE: Deps = Deps::new("_weakset_reduce", &[A("weakref", "WeakSet")]);
pub(crate) static DICT_KEYS_REDUCE: Deps = Deps::new("_dict_keys_reduce", &[G("_make_dict_keys")]);
pub(crate) static DICT_VALUES_REDUCE: Deps =
    Deps::new("_dict_values_reduce", &[G("_make_dict_values")]);
pub(crate) static DICT_ITEMS_REDUCE: Deps =
    Deps::new("_dict_items_reduce", &[G("_make_dict_items")]);
pub(crate) static ODICT_KEYS_REDUCE: Deps =
    Deps::new("_odict_keys_reduce", &[G("_make_dict_keys")]);
pub(crate) static ODICT_VALUES_REDUCE: Deps =
    Deps::new("_odict_values_reduce", &[G("_make_dict_values")]);
pub(crate) static ODICT_ITEMS_REDUCE: Deps =
    Deps::new("_odict_items_reduce", &[G("_make_dict_items")]);
pub(crate) static DATACLASS_FIELD_BASE_REDUCE: Deps = Deps::new(
    "_dataclass_field_base_reduce",
    &[G("_get_dataclass_field_type_sentinel")],
);
pub(crate) static DECOMPOSE_TYPEVAR: Deps =
    Deps::new("_decompose_typevar", &[G("_get_or_create_tracker_id")]);
pub(crate) static TYPEVAR_REDUCE: Deps = Deps::new(
    "_typevar_reduce",
    &[
        G("_lookup_module_and_qualname"),
        G("_make_typevar"),
        G("_decompose_typevar"),
        G("_is_registered_pickle_by_value"),
    ],
);

pub(crate) static PICKLER_DYNAMIC_FUNCTION_REDUCE: Deps = Deps::new(
    "Pickler._dynamic_function_reduce",
    &[
        G("_function_getstate"),
        G("_make_function"),
        G("_function_setstate"),
    ],
);
pub(crate) static PICKLER_FUNCTION_REDUCE: Deps = Deps::new(
    "Pickler._function_reduce",
    &[G("_should_pickle_by_reference")],
);
pub(crate) static PICKLER_FUNCTION_GETNEWARGS: Deps =
    Deps::new("Pickler._function_getnewargs", &[G("_make_empty_cell")]);
pub(crate) static PICKLER_REDUCER_OVERRIDE: Deps = Deps::new(
    "Pickler.reducer_override",
    &[G("_class_reduce"), A("types", "FunctionType")],
);
pub(crate) static PICKLER_SAVE_REDUCE_PICKLE5: Deps = Deps::new(
    "Pickler._save_reduce_pickle5",
    &[
        A("pickle", "TUPLE2"),
        A("pickle", "REDUCE"),
        A("pickle", "POP"),
    ],
);
pub(crate) static PICKLER_SAVE_GLOBAL: Deps = Deps::new(
    "Pickler.save_global",
    &[
        G("_BUILTIN_TYPE_NAMES"),
        G("_builtin_type"),
        G("_should_pickle_by_reference"),
        G("_dynamic_class_reduce"),
    ],
);
pub(crate) static PICKLER_SAVE_FUNCTION: Deps = Deps::new(
    "Pickler.save_function",
    &[
        G("_should_pickle_by_reference"),
        G("PYPY"),
        G("builtin_code_type"),
    ],
);
pub(crate) static PICKLER_SAVE_PYPY_BUILTIN_FUNC: Deps = Deps::new(
    "Pickler.save_pypy_builtin_func",
    &[A("types", "FunctionType")],
);

static ALL: &[&Deps] = &[
    &GET_OR_CREATE_TRACKER_ID,
    &LOOKUP_CLASS_OR_TRACK,
    &REGISTER_PICKLE_BY_VALUE,
    &UNREGISTER_PICKLE_BY_VALUE,
    &LIST_REGISTRY_PICKLE_BY_VALUE,
    &IS_REGISTERED_PICKLE_BY_VALUE,
    &GETATTRIBUTE,
    &WHICHMODULE,
    &SHOULD_PICKLE_BY_REFERENCE,
    &LOOKUP_MODULE_AND_QUALNAME,
    &WALK_GLOBAL_OPS,
    &EXTRACT_CODE_GLOBALS,
    &FIND_IMPORTED_SUBMODULES,
    &IS_TORNADO_COROUTINE,
    &SUBIMPORT,
    &DYNAMIC_SUBIMPORT,
    &BUILTIN_TYPE,
    &MAKE_FUNCTION,
    &MAKE_CELL,
    &MAKE_SKELETON_CLASS,
    &MAKE_SKELETON_ENUM,
    &MAKE_TYPEVAR,
    &MAKE_DICT_KEYS,
    &MAKE_DICT_VALUES,
    &MAKE_DICT_ITEMS,
    &FUNCTION_SETSTATE,
    &CLASS_SETSTATE,
    &GET_DATACLASS_FIELD_TYPE_SENTINEL,
    &CLASS_GETNEWARGS,
    &ENUM_GETNEWARGS,
    &CLASS_GETSTATE,
    &ENUM_GETSTATE,
    &DYNAMIC_CLASS_REDUCE,
    &CLASS_REDUCE,
    &GET_CELL_CONTENTS,
    &FUNCTION_GETSTATE,
    &CELL_REDUCE,
    &FILE_REDUCE,
    &MAPPINGPROXY_REDUCE,
    &MODULE_REDUCE,
    &METHOD_REDUCE,
    &LOGGER_REDUCE,
    &ROOT_LOGGER_REDUCE,
    &WEAKSET_REDUCE,
    &DICT_KEYS_REDUCE,
    &DICT_VALUES_REDUCE,
    &DICT_ITEMS_REDUCE,
    &ODICT_KEYS_REDUCE,
    &ODICT_VALUES_REDUCE,
    &ODICT_ITEMS_REDUCE,
    &DATACLASS_FIELD_BASE_REDUCE,
    &DECOMPOSE_TYPEVAR,
    &TYPEVAR_REDUCE,
    &PICKLER_DYNAMIC_FUNCTION_REDUCE,
    &PICKLER_FUNCTION_REDUCE,
    &PICKLER_FUNCTION_GETNEWARGS,
    &PICKLER_REDUCER_OVERRIDE,
    &PICKLER_SAVE_REDUCE_PICKLE5,
    &PICKLER_SAVE_GLOBAL,
    &PICKLER_SAVE_FUNCTION,
    &PICKLER_SAVE_PYPY_BUILTIN_FUNC,
];

/// Record the values of the globals read by the upstream functions, once the
/// namespace of the module is complete.
pub(crate) fn resolve_all(py: Python<'_>, module_dict: &Bound<'_, PyDict>) -> PyResult<()> {
    for deps in ALL {
        deps.resolve(py, module_dict)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The upstream functions
// ---------------------------------------------------------------------------

/// Upstream's `cloudpickle/cloudpickle.py` (cloudpickle commit 4af5936).
const UPSTREAM_SOURCE: &str = include_str!("upstream/cloudpickle.py");
const UPSTREAM_FILENAME: &str = "<cloudpickle_rust: upstream cloudpickle/cloudpickle.py>";

/// The number of the line of upstream's `cloudpickle.py` where `start` (the
/// beginning of a line) is (the `__firstlineno__` of the classes defined
/// there on Python 3.13+), and the definition that starts there: up to the
/// next two blank lines.
pub(crate) fn upstream_definition(start: &str) -> PyResult<(usize, &'static str)> {
    let offset = UPSTREAM_SOURCE
        .find(&format!("\n{start}"))
        .ok_or_else(|| PyRuntimeError::new_err(format!("{start:?} is not in upstream's source")))?;
    // The definition starts after the newline at `offset`.
    let line = UPSTREAM_SOURCE[..=offset].matches('\n').count() + 1;
    let definition = &UPSTREAM_SOURCE[offset + 1..];
    // Up to the end of its last line.
    let end = definition
        .find("\n\n\n")
        .map_or(definition.len(), |end| end + 1);
    Ok((line, &definition[..end]))
}

static UPSTREAM_FUNCTIONS: PyOnceLock<Py<PyDict>> = PyOnceLock::new();

fn upstream_function<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    let functions = UPSTREAM_FUNCTIONS.get_or_try_init(py, || build_upstream_functions(py))?;
    functions
        .bind(py)
        .get_item(name)?
        .ok_or_else(|| PyRuntimeError::new_err(format!("no upstream implementation of {name}")))
}

/// The default values of the parameters of the upstream functions (evaluated
/// when upstream's module is imported, hence the values of the module
/// initialization).
fn upstream_defaults<'py>(py: Python<'py>, qualname: &str) -> PyResult<Bound<'py, PyAny>> {
    let g = globals(py);
    let none = || py.None();
    Ok(match qualname {
        "_should_pickle_by_reference" | "_lookup_module_and_qualname" | "Pickler.save_function" => {
            (none(),).into_pyobject(py)?.into_any()
        }
        "_make_cell" => (g.empty_cell_value.bind(py),).into_pyobject(py)?.into_any(),
        "_make_dict_keys" | "_make_dict_values" | "_make_dict_items" => {
            (false,).into_pyobject(py)?.into_any()
        }
        "dump" | "dumps" | "Pickler.__init__" => (none(), none()).into_pyobject(py)?.into_any(),
        "Pickler._save_reduce_pickle5" => (none(), none(), none(), none(), none())
            .into_pyobject(py)?
            .into_any(),
        "Pickler.save_global" => (none(), g.struct_pack.bind(py))
            .into_pyobject(py)?
            .into_any(),
        _ => none().into_bound(py),
    })
}

/// Compile upstream's module and create its functions (and the methods of its
/// `Pickler` class) with the namespace of `cloudpickle_rust.cloudpickle` as
/// their globals.
fn build_upstream_functions(py: Python<'_>) -> PyResult<Py<PyDict>> {
    let g = globals(py);
    let module_code = g
        .builtins
        .bind(py)
        .getattr(intern!(py, "compile"))?
        .call1((UPSTREAM_SOURCE, UPSTREAM_FILENAME, "exec"))?;
    let code_type = g.code_type.bind(py);
    let function_type = g.function_type.bind(py);
    let module_dict = g.module_dict.bind(py);
    let functions = PyDict::new(py);
    let define = |code: &Bound<'_, PyAny>, qualname: String| -> PyResult<()> {
        let name = code.getattr(intern!(py, "co_name"))?;
        let closure = if code.getattr(intern!(py, "co_freevars"))?.len()? == 0 {
            py.None().into_bound(py)
        } else {
            // The `__class__` cell of the methods calling `super()`.
            let cell = crate::reconstructors::make_empty_cell(py)?;
            cell.setattr(intern!(py, "cell_contents"), g.pickler_type.bind(py))?;
            PyTuple::new(py, [cell])?.into_any()
        };
        let defaults = upstream_defaults(py, &qualname)?;
        let function = function_type.call1((code, module_dict, name, defaults, closure))?;
        function.setattr(intern!(py, "__qualname__"), &qualname)?;
        functions.set_item(qualname, function)
    };
    for constant in module_code.getattr(intern!(py, "co_consts"))?.try_iter()? {
        let constant = constant?;
        if !constant.is_instance(code_type)? {
            continue;
        }
        let name: String = constant.getattr(intern!(py, "co_name"))?.extract()?;
        match name.as_str() {
            "Pickler" => {
                for method in constant.getattr(intern!(py, "co_consts"))?.try_iter()? {
                    let method = method?;
                    if method.is_instance(code_type)? {
                        let method_name: String =
                            method.getattr(intern!(py, "co_name"))?.extract()?;
                        define(&method, format!("Pickler.{method_name}"))?;
                    }
                }
            }
            // The body of the class of the sentinel (created at initialization).
            "_empty_cell_value" => {}
            "_getattribute" => {
                // Defined differently for Python 3.14 and later.
                let splits = constant
                    .getattr(intern!(py, "co_names"))?
                    .contains(intern!(py, "split"))?;
                if splits == g.at_least(3, 14) {
                    define(&constant, name)?;
                }
            }
            _ => define(&constant, name)?,
        }
    }
    Ok(functions.unbind())
}

// ---------------------------------------------------------------------------
// Iterable unpacking
// ---------------------------------------------------------------------------

const HELPERS_SOURCE: &str = "\
def unpack2(obj):
    a, b = obj
    return a, b


def unpack4(obj):
    a, b, c, d = obj
    return a, b, c, d
";

static HELPERS: PyOnceLock<Py<PyDict>> = PyOnceLock::new();

fn helper<'py>(py: Python<'py>, name: &Bound<'py, PyString>) -> PyResult<Bound<'py, PyAny>> {
    let helpers = HELPERS.get_or_try_init(py, || -> PyResult<Py<PyDict>> {
        let namespace = PyDict::new(py);
        namespace.set_item("__builtins__", py.import("builtins")?)?;
        py.run(
            &std::ffi::CString::new(HELPERS_SOURCE).expect("no NUL byte"),
            Some(&namespace),
            None,
        )?;
        Ok(namespace.unbind())
    })?;
    helpers
        .bind(py)
        .get_item(name)?
        .ok_or_else(|| PyRuntimeError::new_err("missing unpacking helper"))
}

/// `a, b = obj` (or `a, b, c, d = obj`) with the exact semantics, including
/// the error messages, of the running Python version.
pub(crate) fn unpack<'py>(obj: &Bound<'py, PyAny>, n: usize) -> PyResult<Vec<Bound<'py, PyAny>>> {
    if let Ok(t) = obj.cast_exact::<PyTuple>() {
        if t.len() == n {
            return Ok(t.iter().collect());
        }
    }
    let py = obj.py();
    let name = match n {
        2 => intern!(py, "unpack2"),
        4 => intern!(py, "unpack4"),
        _ => return Err(PyRuntimeError::new_err("unsupported unpacking")),
    };
    let values = helper(py, name)?.call1((obj,))?;
    Ok(values.cast_into::<PyTuple>()?.iter().collect())
}

// ---------------------------------------------------------------------------
// Global variables
// ---------------------------------------------------------------------------

/// `LOAD_GLOBAL name` in the namespace of the module: the value of its global
/// variable `name`, else of the builtin `name`, else `NameError`.
pub(crate) fn load_global<'py>(
    py: Python<'py>,
    name: &Bound<'py, PyString>,
) -> PyResult<Bound<'py, PyAny>> {
    let g = globals(py);
    if let Some(value) = g.module_dict.bind(py).get_item(name)? {
        return Ok(value);
    }
    if let Some(value) = g.builtins_dict.bind(py).get_item(name)? {
        return Ok(value);
    }
    let err = pyo3::exceptions::PyNameError::new_err(format!("name '{name}' is not defined"));
    if g.at_least(3, 10) {
        // Set by the interpreter since Python 3.10 (used for suggestions).
        err.value(py).setattr(intern!(py, "name"), name)?;
    }
    Err(err)
}
