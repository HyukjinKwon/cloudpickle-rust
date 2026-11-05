//! The `Pickler` class and the `dump` / `dumps` shorthands.
//!
//! `Pickler` is created at import time as a Python-level subclass of
//! `pickle.Pickler` (the C `_pickle.Pickler` on CPython), exactly like the
//! upstream class statement, with methods implemented in Rust (see
//! `utils::add_method`).

use pyo3::exceptions::{PyKeyError, PyRecursionError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyInt, PyList, PyModule, PyString, PySuper, PyTuple, PyType};

use crate::lookup::should_pickle_by_reference;
use crate::reconstructors::make_empty_cell;
use crate::reducers::{class_reduce, dynamic_class_reduce, function_getstate};
use crate::state::globals;
use crate::utils::{add_function, add_method, fastcall_method, module_global, raise_from, Params};

type R<'py> = PyResult<Bound<'py, PyAny>>;
type Args<'py> = [Option<Bound<'py, PyAny>>];

/// A required parameter (always present once the arguments are parsed).
fn arg<'a, 'py>(args: &'a Args<'py>, i: usize) -> &'a Bound<'py, PyAny> {
    args[i].as_ref().expect("missing required argument")
}

/// An optional parameter whose default value is None.
fn arg_or_none<'py>(py: Python<'py>, args: &Args<'py>, i: usize) -> Bound<'py, PyAny> {
    args[i].clone().unwrap_or_else(|| py.None().into_bound(py))
}

fn none(py: Python<'_>) -> Bound<'_, PyAny> {
    py.None().into_bound(py)
}

fn super_of<'py>(slf: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PySuper>> {
    let py = slf.py();
    PySuper::new(globals(py).pickler_type.bind(py), slf)
}

fn kwargs<'py>(
    py: Python<'py>,
    items: &[(&Bound<'py, PyString>, &Bound<'py, PyAny>)],
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    for (k, v) in items {
        d.set_item(k, v)?;
    }
    Ok(d)
}

// ---------------------------------------------------------------------------
// Methods shared by all Python implementations
// ---------------------------------------------------------------------------

const DYNAMIC_FUNCTION_REDUCE: Params = Params {
    qualname: "Pickler._dynamic_function_reduce",
    names: &["func"],
    required: 1,
};

/// Reduce a function that is not pickleable via attribute lookup.
fn dynamic_function_reduce<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let func = arg(args, 0);
    let py = func.py();
    let g = globals(py);
    let newargs = slf.call_method1(intern!(py, "_function_getnewargs"), (func,))?;
    let state = function_getstate(func)?;
    Ok((
        g.make_function.bind(py),
        newargs,
        state,
        py.None(),
        py.None(),
        g.function_setstate.bind(py),
    )
        .into_pyobject(py)?
        .into_any())
}

const FUNCTION_REDUCE: Params = Params {
    qualname: "Pickler._function_reduce",
    names: &["obj"],
    required: 1,
};

/// Reducer for function objects.
///
/// If obj is a top-level attribute of a file-backed module, this reducer
/// returns NotImplemented, making the cloudpickle.Pickler fall back to
/// traditional pickle.Pickler routines to save obj. Otherwise, it reduces
/// obj using a custom cloudpickle reducer designed specifically to handle
/// dynamic functions.
fn function_reduce<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let obj = arg(args, 0);
    let py = obj.py();
    if should_pickle_by_reference(obj, None)? {
        Ok(py.NotImplemented().into_bound(py))
    } else {
        slf.call_method1(intern!(py, "_dynamic_function_reduce"), (obj,))
    }
}

const FUNCTION_GETNEWARGS: Params = Params {
    qualname: "Pickler._function_getnewargs",
    names: &["func"],
    required: 1,
};

fn function_getnewargs<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let func = arg(args, 0);
    let py = func.py();
    let code = func.getattr(intern!(py, "__code__"))?;

    // base_globals represents the future global namespace of func at
    // unpickling time. Looking it up and storing it in
    // cloudpickle.Pickler.globals_ref allow functions sharing the same
    // globals at pickling time to also share them once unpickled, at one
    // condition: since globals_ref is an attribute of a cloudpickle.Pickler
    // instance, and that a new cloudpickle.Pickler is created each time
    // cloudpickle.dump or cloudpickle.dumps is called, functions also need
    // to be saved within the same invocation of
    // cloudpickle.dump/cloudpickle.dumps (for example:
    // cloudpickle.dumps([f1, f2])). There is no such limitation when using
    // cloudpickle.Pickler.dump, as long as the multiple invocations are
    // bound to the same cloudpickle.Pickler instance.
    let globals_ref = slf.getattr(intern!(py, "globals_ref"))?;
    let func_globals = func.getattr(intern!(py, "__globals__"))?;
    let key = crate::utils::py_id(&func_globals)?;
    let base_globals = match globals_ref.cast_exact::<PyDict>() {
        Ok(globals_ref) => match globals_ref.get_item(&key)? {
            Some(base_globals) => base_globals,
            None => {
                let base_globals = PyDict::new(py).into_any();
                globals_ref.set_item(&key, &base_globals)?;
                base_globals
            }
        },
        Err(_) => globals_ref.call_method1(intern!(py, "setdefault"), (&key, PyDict::new(py)))?,
    };

    if base_globals.eq(PyDict::new(py))? {
        // Add module attributes used to resolve relative imports
        // instructions inside func.
        for k in [
            intern!(py, "__package__"),
            intern!(py, "__name__"),
            intern!(py, "__path__"),
            intern!(py, "__file__"),
        ] {
            if func_globals.contains(k)? {
                base_globals.set_item(k, func_globals.get_item(k)?)?;
            }
        }
    }

    // Do not bind the free variables before the function is created to
    // avoid infinite recursion.
    let closure = if func.getattr(intern!(py, "__closure__"))?.is_none() {
        none(py)
    } else {
        let n = code.getattr(intern!(py, "co_freevars"))?.len()?;
        let cells = (0..n)
            .map(|_| make_empty_cell(py))
            .collect::<PyResult<Vec<_>>>()?;
        PyTuple::new(py, cells)?.into_any()
    };

    Ok((code, base_globals, py.None(), py.None(), closure)
        .into_pyobject(py)?
        .into_any())
}

const DUMP: Params = Params {
    qualname: "Pickler.dump",
    names: &["obj"],
    required: 1,
};

fn dump_method<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let obj = arg(args, 0);
    let py = obj.py();
    match super_of(slf)?.call_method1(intern!(py, "dump"), (obj,)) {
        Ok(result) => Ok(result),
        Err(err) if err.is_instance_of::<PyRecursionError>(py) => {
            let msg = "Could not pickle object as excessively deep recursion required.";
            let new = PyErr::from_type(globals(py).pickling_error.bind(py).clone(), msg);
            Err(raise_from(py, new, err))
        }
        Err(err) => Err(err),
    }
}

const INIT: Params = Params {
    qualname: "Pickler.__init__",
    names: &["file", "protocol", "buffer_callback"],
    required: 1,
};

fn init_method<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let py = slf.py();
    let file = arg(args, 0);
    let mut protocol = arg_or_none(py, args, 1);
    let buffer_callback = arg_or_none(py, args, 2);
    if protocol.is_none() {
        protocol = module_global(py, intern!(py, "DEFAULT_PROTOCOL"))?;
    }
    let kw = kwargs(
        py,
        &[
            (intern!(py, "protocol"), &protocol),
            (intern!(py, "buffer_callback"), &buffer_callback),
        ],
    )?;
    super_of(slf)?
        .getattr(intern!(py, "__init__"))?
        .call((file,), Some(&kw))?;
    // map functions __globals__ attribute ids, to ensure that functions
    // sharing the same global namespace at pickling time also share
    // their global namespace at unpickling time.
    slf.setattr(intern!(py, "globals_ref"), PyDict::new(py))?;
    slf.setattr(
        intern!(py, "proto"),
        py.get_type::<PyInt>().call1((&protocol,))?,
    )?;
    Ok(none(py))
}

// ---------------------------------------------------------------------------
// CPython: reducer_override
// ---------------------------------------------------------------------------

const REDUCER_OVERRIDE: Params = Params {
    qualname: "Pickler.reducer_override",
    names: &["obj"],
    required: 1,
};

/// Type-agnostic reducing callback for function and classes.
fn reducer_override<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let obj = arg(args, 0);
    let py = obj.py();
    // `issubclass(type(obj), type)`
    if obj.is_instance_of::<PyType>() {
        return class_reduce(obj);
    }
    if obj.is_instance(globals(py).function_type.bind(py))? {
        return slf.call_method1(intern!(py, "_function_reduce"), (obj,));
    }
    // fallback to save_global, including the Pickler's
    // dispatch_table
    Ok(py.NotImplemented().into_bound(py))
}

/// `reducer_override` of the PyPy implementation.
///
/// On CPython, the functions of this module (the reconstructors such as
/// `_make_function`, `subimport`...) are saved by reference as module
/// attributes: their `__reduce__` returns their name. On PyPy, cpyext
/// functions reduce to `getattr(module, name)` instead, and the module itself
/// reduces to `subimport(name)`, i.e. to one of these functions, which would
/// recurse forever: reduce them to their name like on CPython. Everything else
/// is left to the dispatch-based savers below, like in upstream.
fn pypy_reducer_override<'py>(_slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let obj = arg(args, 0);
    let py = obj.py();
    let g = globals(py);
    if obj.get_type().is(g.builtin_function_type.bind(py)) {
        if let Some(owner) = obj.getattr_opt(intern!(py, "__self__"))? {
            if owner.is(g.module.bind(py)) {
                return obj.getattr(intern!(py, "__name__"));
            }
        }
    }
    Ok(py.NotImplemented().into_bound(py))
}

// ---------------------------------------------------------------------------
// PyPy: the pure-Python pickler has no reducer_override support in upstream,
// which hacks the savers of functions and classes instead.
// ---------------------------------------------------------------------------

const SAVE_REDUCE_PICKLE5: Params = Params {
    qualname: "Pickler._save_reduce_pickle5",
    names: &[
        "func",
        "args",
        "state",
        "listitems",
        "dictitems",
        "state_setter",
        "obj",
    ],
    required: 2,
};

fn save_reduce_pickle5<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let py = slf.py();
    let g = globals(py);
    let func = arg(args, 0);
    let func_args = arg(args, 1);
    let state = arg_or_none(py, args, 2);
    let listitems = arg_or_none(py, args, 3);
    let dictitems = arg_or_none(py, args, 4);
    let state_setter = arg_or_none(py, args, 5);
    let obj = arg_or_none(py, args, 6);

    let save = slf.getattr(intern!(py, "save"))?;
    let write = slf.getattr(intern!(py, "write"))?;
    let none_value = none(py);
    let kw = kwargs(
        py,
        &[
            (intern!(py, "state"), &none_value),
            (intern!(py, "listitems"), &listitems),
            (intern!(py, "dictitems"), &dictitems),
            (intern!(py, "obj"), &obj),
        ],
    )?;
    slf.getattr(intern!(py, "save_reduce"))?
        .call((func, func_args), Some(&kw))?;
    // backport of the Python 3.8 state_setter pickle operations
    save.call1((state_setter,))?;
    save.call1((&obj,))?; // simple BINGET opcode as obj is already memoized.
    save.call1((state,))?;
    let pickle = g.pickle_module.bind(py);
    write.call1((pickle.getattr(intern!(py, "TUPLE2"))?,))?;
    // Trigger a state_setter(obj, state) function call.
    write.call1((pickle.getattr(intern!(py, "REDUCE"))?,))?;
    // The purpose of state_setter is to carry-out an
    // inplace modification of obj. We do not care about what the
    // method might return, so its output is eventually removed from
    // the stack.
    write.call1((pickle.getattr(intern!(py, "POP"))?,))?;
    Ok(none(py))
}

fn save_reduce_with_obj<'py>(
    slf: &Bound<'py, PyAny>,
    func: &Bound<'py, PyAny>,
    func_args: Bound<'py, PyAny>,
    obj: &Bound<'py, PyAny>,
) -> R<'py> {
    let py = slf.py();
    let kw = kwargs(py, &[(intern!(py, "obj"), obj)])?;
    slf.getattr(intern!(py, "save_reduce"))?
        .call((func, func_args), Some(&kw))
}

fn save_reduce_pickle5_of<'py>(
    slf: &Bound<'py, PyAny>,
    reduced: &Bound<'py, PyAny>,
    obj: &Bound<'py, PyAny>,
) -> R<'py> {
    let py = slf.py();
    let kw = kwargs(py, &[(intern!(py, "obj"), obj)])?;
    let reduced = PyTuple::new(py, reduced.try_iter()?.collect::<PyResult<Vec<_>>>()?)?;
    slf.getattr(intern!(py, "_save_reduce_pickle5"))?
        .call(reduced, Some(&kw))
}

fn super_save_global<'py>(
    slf: &Bound<'py, PyAny>,
    obj: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
) -> R<'py> {
    let py = slf.py();
    let kw = kwargs(py, &[(intern!(py, "name"), name)])?;
    super_of(slf)?
        .getattr(intern!(py, "save_global"))?
        .call((obj,), Some(&kw))
}

const SAVE_GLOBAL: Params = Params {
    qualname: "Pickler.save_global",
    names: &["obj", "name", "pack"],
    required: 1,
};

/// Main dispatch method.
///
/// The name of this method is somewhat misleading: all types get
/// dispatched here.
fn save_global<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let py = slf.py();
    let g = globals(py);
    let obj = arg(args, 0);
    let name = arg_or_none(py, args, 1);
    let type_type = py.get_type::<PyType>().into_any();
    if obj.is(g.none_type.bind(py)) {
        return save_reduce_with_obj(
            slf,
            &type_type,
            (py.None(),).into_pyobject(py)?.into_any(),
            obj,
        );
    }
    if obj.is(g.ellipsis_type.bind(py)) {
        return save_reduce_with_obj(
            slf,
            &type_type,
            (py.Ellipsis(),).into_pyobject(py)?.into_any(),
            obj,
        );
    }
    if obj.is(g.notimplemented_type.bind(py)) {
        return save_reduce_with_obj(
            slf,
            &type_type,
            (py.NotImplemented(),).into_pyobject(py)?.into_any(),
            obj,
        );
    }
    if let Some(type_name) = g.builtin_type_names.bind(py).get_item(obj)? {
        return save_reduce_with_obj(
            slf,
            g.builtin_type.bind(py),
            (type_name,).into_pyobject(py)?.into_any(),
            obj,
        );
    }

    if !name.is_none() {
        super_save_global(slf, obj, &name)?;
    } else if !should_pickle_by_reference(obj, Some(&name))? {
        save_reduce_pickle5_of(slf, &dynamic_class_reduce(obj)?, obj)?;
    } else {
        super_save_global(slf, obj, &name)?;
    }
    Ok(none(py))
}

const SAVE_FUNCTION: Params = Params {
    qualname: "Pickler.save_function",
    names: &["obj", "name"],
    required: 1,
};

/// Registered with the dispatch to handle all function types.
///
/// Determines what kind of function obj is (e.g. lambda, defined at
/// interactive prompt, etc) and handles the pickling appropriately.
fn save_function<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let py = slf.py();
    let g = globals(py);
    let obj = arg(args, 0);
    let name = arg_or_none(py, args, 1);
    if should_pickle_by_reference(obj, Some(&name))? {
        super_save_global(slf, obj, &name)
    } else if g.pypy
        && obj
            .getattr(intern!(py, "__code__"))?
            .is_instance(g.builtin_code_type.bind(py))?
    {
        slf.call_method1(intern!(py, "save_pypy_builtin_func"), (obj,))
    } else {
        let reduced = slf.call_method1(intern!(py, "_dynamic_function_reduce"), (obj,))?;
        save_reduce_pickle5_of(slf, &reduced, obj)
    }
}

const SAVE_PYPY_BUILTIN_FUNC: Params = Params {
    qualname: "Pickler.save_pypy_builtin_func",
    names: &["obj"],
    required: 1,
};

/// Save pypy equivalent of builtin functions.
fn save_pypy_builtin_func<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let py = slf.py();
    let g = globals(py);
    let obj = arg(args, 0);
    let func_args = (
        obj.getattr(intern!(py, "__code__"))?,
        PyDict::new(py),
        obj.getattr(intern!(py, "__name__"))?,
        obj.getattr(intern!(py, "__defaults__"))?,
        obj.getattr(intern!(py, "__closure__"))?,
    )
        .into_pyobject(py)?;
    let kw = kwargs(py, &[(intern!(py, "obj"), obj)])?;
    slf.getattr(intern!(py, "save_reduce"))?.call(
        (
            g.function_type.bind(py),
            func_args,
            obj.getattr(intern!(py, "__dict__"))?,
        ),
        Some(&kw),
    )?;
    Ok(none(py))
}

// ---------------------------------------------------------------------------
// Dispatch table
// ---------------------------------------------------------------------------

const DISPATCH_TABLE_GETITEM: Params = Params {
    qualname: "_DispatchTable.__getitem__",
    names: &["key"],
    required: 1,
};

/// `ChainMap.__getitem__`, without the cost of raising and catching
/// `KeyError`s in Python for every missing mapping.
fn dispatch_table_getitem<'py>(slf: &Bound<'py, PyAny>, args: &Args<'py>) -> R<'py> {
    let key = arg(args, 0);
    let py = key.py();
    let lookup = |mapping: &Bound<'py, PyAny>| -> PyResult<Option<Bound<'py, PyAny>>> {
        if let Ok(mapping) = mapping.cast_exact::<PyDict>() {
            return mapping.get_item(key);
        }
        // can't use 'key in mapping' with defaultdict
        match mapping.get_item(key) {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.is_instance_of::<PyKeyError>(py) => Ok(None),
            Err(err) => Err(err),
        }
    };
    let maps = slf.getattr(intern!(py, "maps"))?;
    if let Ok(maps) = maps.cast_exact::<PyList>() {
        let mut i = 0;
        while i < maps.len() {
            if let Some(value) = lookup(&maps.get_item(i)?)? {
                return Ok(value);
            }
            i += 1;
        }
    } else {
        for mapping in maps.try_iter()? {
            if let Some(value) = lookup(&mapping?)? {
                return Ok(value);
            }
        }
    }
    let g = globals(py);
    if slf.get_type().is(g.dispatch_table_type.bind(py)) {
        // ChainMap.__missing__
        Err(PyKeyError::new_err((key.clone().unbind(),)))
    } else {
        // support subclasses that define __missing__
        slf.call_method1(intern!(py, "__missing__"), (key,))
    }
}

// ---------------------------------------------------------------------------
// C entry points
// ---------------------------------------------------------------------------

fastcall_method!(
    dynamic_function_reduce_c,
    DYNAMIC_FUNCTION_REDUCE,
    dynamic_function_reduce
);
fastcall_method!(function_reduce_c, FUNCTION_REDUCE, function_reduce);
fastcall_method!(
    function_getnewargs_c,
    FUNCTION_GETNEWARGS,
    function_getnewargs
);
fastcall_method!(dump_c, DUMP, dump_method);
fastcall_method!(init_c, INIT, init_method);
fastcall_method!(reducer_override_c, REDUCER_OVERRIDE, reducer_override);
fastcall_method!(
    pypy_reducer_override_c,
    REDUCER_OVERRIDE,
    pypy_reducer_override
);
fastcall_method!(
    save_reduce_pickle5_c,
    SAVE_REDUCE_PICKLE5,
    save_reduce_pickle5
);
fastcall_method!(save_global_c, SAVE_GLOBAL, save_global);
fastcall_method!(save_function_c, SAVE_FUNCTION, save_function);
fastcall_method!(
    save_pypy_builtin_func_c,
    SAVE_PYPY_BUILTIN_FUNC,
    save_pypy_builtin_func
);
fastcall_method!(
    dispatch_table_getitem_c,
    DISPATCH_TABLE_GETITEM,
    dispatch_table_getitem
);

const REDUCER_OVERRIDE_DOC: &std::ffi::CStr = c"reducer_override($self, obj)
--

Type-agnostic reducing callback for function and classes.

For performance reasons, subclasses of the C `pickle.Pickler` class
cannot register custom reducers for functions and classes in the
dispatch_table attribute. Reducers for such types must instead
implemented via the special `reducer_override` method.

Note that this method will be called for any object except a few
builtin-types (int, lists, dicts etc.), which differs from reducers
in the Pickler's dispatch_table, each of them being invoked for
objects of a specific type only.

This property comes in handy for classes: although most classes are
instances of the ``type`` metaclass, some of them can be instances
of other custom metaclasses (such as enum.EnumMeta for example). In
particular, the metaclass will likely not be known in advance, and
thus cannot be special-cased using an entry in the dispatch_table.
reducer_override, among other things, allows us to register a
reducer that will be called for any class, independently of its
type.

Notes:

* reducer_override has the priority over dispatch_table-registered
reducers.
* reducer_override can be used to fix other limitations of
  cloudpickle for other types that suffered from type-specific
  reducers, such as Exceptions. See
  https://github.com/cloudpipe/cloudpickle/issues/248";

const FUNCTION_REDUCE_DOC: &std::ffi::CStr = c"_function_reduce($self, obj)
--

Reducer for function objects.

If obj is a top-level attribute of a file-backed module, this reducer
returns NotImplemented, making the cloudpickle.Pickler fall back to
traditional pickle.Pickler routines to save obj. Otherwise, it reduces
obj using a custom cloudpickle reducer designed specifically to handle
dynamic functions.";

const SAVE_PYPY_BUILTIN_FUNC_DOC: &std::ffi::CStr = c"save_pypy_builtin_func($self, obj)
--

Save pypy equivalent of builtin functions.

PyPy does not have the concept of builtin-functions. Instead,
builtin-functions are simple function instances, but with a
builtin-code attribute.
Most of the time, builtin functions should be pickled by attribute.
But PyPy has flaky support for __qualname__, so some builtin
functions such as float.__new__ will be classified as dynamic. For
this reason only, we created this special routine. Because
builtin-functions are not expected to have closure or globals,
there is no additional hack (compared the one already implemented
in pickle) to protect ourselves from reference cycles. A simple
(reconstructor, newargs, obj.__dict__) tuple is save_reduced.  Note
also that PyPy improved their support for __qualname__ in v3.6, so
this routing should be removed when cloudpickle supports only PyPy
3.6 and later.";

/// Create the dispatch table and the `Pickler` class.
///
/// Returns `(Pickler, _DispatchTable)`.
pub(crate) fn create_pickler_class<'py>(
    m: &Bound<'py, PyModule>,
    dispatch: &Bound<'py, PyDict>,
    pypy: bool,
) -> PyResult<(Bound<'py, PyType>, Bound<'py, PyType>)> {
    let py = m.py();
    let module_name = m.name()?;
    let type_type = py.get_type::<PyType>();

    // `ChainMap` subclass whose lookups are implemented natively.
    let chain_map = py.import("collections")?.getattr("ChainMap")?;
    let ns = PyDict::new(py);
    ns.set_item("__module__", &module_name)?;
    ns.set_item("__qualname__", "_DispatchTable")?;
    ns.set_item(
        "__doc__",
        "ChainMap used as the dispatch_table of cloudpickle's Pickler.\n\n\
         Same as collections.ChainMap but with a native __getitem__, which is\n\
         called by the pickler for every object not handled by reducer_override.",
    )?;
    ns.set_item("__slots__", PyTuple::empty(py))?;
    let dispatch_table_type = type_type
        .call1(("_DispatchTable", (chain_map,), ns))?
        .cast_into::<PyType>()?;
    add_method(
        &dispatch_table_type,
        c"__getitem__",
        dispatch_table_getitem_c,
        c"__getitem__($self, key)\n--\n\n",
    )?;
    let copyreg_dispatch_table = py.import("copyreg")?.getattr("dispatch_table")?;
    let dispatch_table = dispatch_table_type.call1((dispatch, copyreg_dispatch_table))?;

    let pickle = py.import("pickle")?;
    let mut base = pickle.getattr("Pickler")?;
    if pypy && !base.hasattr("dispatch")? {
        // Upstream's PyPy implementation (below) customizes the pure-Python
        // pickler, which was `pickle.Pickler` on PyPy until PyPy 3.11 started
        // to ship a native `_pickle` module (in which upstream fails at
        // import time). This native pickler cannot be used either: it does not
        // memoize objects reduced with a `state_setter` before saving their
        // state (infinite recursion on dynamic functions and classes). Keep
        // using the pure-Python pickler, which is still available.
        base = pickle.getattr("_Pickler")?;
    }
    let ns = PyDict::new(py);
    ns.set_item("__module__", &module_name)?;
    ns.set_item("__qualname__", "Pickler")?;
    ns.set_item("__doc__", py.None())?;
    // set of reducers defined and used by cloudpickle (private)
    ns.set_item("_dispatch_table", dispatch)?;
    ns.set_item("dispatch_table", &dispatch_table)?;
    let pickler = type_type
        .call1(("Pickler", (&base,), ns))?
        .cast_into::<PyType>()?;

    // function reducers are defined as instance methods of cloudpickle.Pickler
    // objects, as they rely on a cloudpickle.Pickler attribute (globals_ref)
    add_method(
        &pickler,
        c"_dynamic_function_reduce",
        dynamic_function_reduce_c,
        c"_dynamic_function_reduce($self, func)\n--\n\nReduce a function that is not pickleable via attribute lookup.",
    )?;
    add_method(
        &pickler,
        c"_function_reduce",
        function_reduce_c,
        FUNCTION_REDUCE_DOC,
    )?;
    add_method(
        &pickler,
        c"_function_getnewargs",
        function_getnewargs_c,
        c"_function_getnewargs($self, func)\n--\n\n",
    )?;
    add_method(&pickler, c"dump", dump_c, c"dump($self, obj)\n--\n\n")?;
    add_method(
        &pickler,
        c"__init__",
        init_c,
        c"__init__($self, file, protocol=None, buffer_callback=None)\n--\n\n",
    )?;

    if !pypy {
        // pickle.Pickler is the C implementation of the CPython pickler and
        // therefore we rely on reduce_override method to customize the pickler
        // behavior.

        // `cloudpickle.Pickler.dispatch` is only left for backward
        // compatibility - note that when using protocol 5,
        // `cloudpickle.Pickler.dispatch` is not an extension of
        // `pickle._Pickler.dispatch` dictionary, because `cloudpickle.Pickler`
        // subclasses the C-implemented `pickle.Pickler`, which does not expose
        // a `dispatch` attribute.  Earlier versions of `cloudpickle.Pickler`
        // used `cloudpickle.Pickler.dispatch` as a class-level attribute
        // storing all reducers implemented by cloudpickle, but the attribute
        // name was not a great choice given because it would collide with a
        // similarly named attribute in the pure-Python `pickle._Pickler`
        // implementation in the standard library.
        pickler.setattr("dispatch", &dispatch_table)?;

        // Implementation of the reducer_override callback, in order to
        // efficiently serialize dynamic functions and classes by subclassing
        // the C-implemented `pickle.Pickler`.
        add_method(
            &pickler,
            c"reducer_override",
            reducer_override_c,
            REDUCER_OVERRIDE_DOC,
        )?;
    } else {
        // When reducer_override is not available, hack the pure-Python
        // Pickler's types.FunctionType and type savers. Note: the type saver
        // must override Pickler.save_global, because pickle.py contains a
        // hard-coded call to save_global when pickling meta-classes.
        let dispatch = base.getattr("dispatch")?.call_method0("copy")?;
        pickler.setattr("dispatch", &dispatch)?;
        add_method(
            &pickler,
            c"_save_reduce_pickle5",
            save_reduce_pickle5_c,
            c"_save_reduce_pickle5($self, func, args, state=None, listitems=None, dictitems=None, state_setter=None, obj=None)\n--\n\n",
        )?;
        let save_global = add_method(
            &pickler,
            c"save_global",
            save_global_c,
            c"save_global($self, obj, name=None, pack=None)\n--\n\nMain dispatch method.\n\nThe name of this method is somewhat misleading: all types get\ndispatched here.",
        )?;
        dispatch.set_item(&type_type, save_global)?;
        let save_function = add_method(
            &pickler,
            c"save_function",
            save_function_c,
            c"save_function($self, obj, name=None)\n--\n\nRegistered with the dispatch to handle all function types.\n\nDetermines what kind of function obj is (e.g. lambda, defined at\ninteractive prompt, etc) and handles the pickling appropriately.",
        )?;
        add_method(
            &pickler,
            c"save_pypy_builtin_func",
            save_pypy_builtin_func_c,
            SAVE_PYPY_BUILTIN_FUNC_DOC,
        )?;
        let function_type = py.import("types")?.getattr("FunctionType")?;
        dispatch.set_item(function_type, save_function)?;
        add_method(
            &pickler,
            c"reducer_override",
            pypy_reducer_override_c,
            c"reducer_override($self, obj)\n--\n\nSave the functions of this module by reference (cpyext functions do not\nreduce to their name on PyPy).",
        )?;
    }
    Ok((pickler, dispatch_table_type))
}

// ---------------------------------------------------------------------------
// Shorthands similar to pickle.dump/pickle.dumps
// ---------------------------------------------------------------------------

fn pickler_kwargs<'py>(
    py: Python<'py>,
    protocol: Option<&Bound<'py, PyAny>>,
    buffer_callback: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyDict>> {
    let protocol = protocol.cloned().unwrap_or_else(|| none(py));
    let buffer_callback = buffer_callback.cloned().unwrap_or_else(|| none(py));
    kwargs(
        py,
        &[
            (intern!(py, "protocol"), &protocol),
            (intern!(py, "buffer_callback"), &buffer_callback),
        ],
    )
}

/// Serialize obj as bytes streamed into file
///
/// protocol defaults to cloudpickle.DEFAULT_PROTOCOL which is an alias to
/// pickle.HIGHEST_PROTOCOL. This setting favors maximum communication
/// speed between processes running the same Python version.
///
/// Set protocol=pickle.DEFAULT_PROTOCOL instead if you need to ensure
/// compatibility with older versions of Python (although this is not always
/// guaranteed to work because cloudpickle relies on some internal
/// implementation details that can change from one Python version to the
/// next).
#[pyfunction]
#[pyo3(signature = (obj, file, protocol=None, buffer_callback=None))]
fn dump<'py>(
    obj: &Bound<'py, PyAny>,
    file: &Bound<'py, PyAny>,
    protocol: Option<&Bound<'py, PyAny>>,
    buffer_callback: Option<&Bound<'py, PyAny>>,
) -> PyResult<()> {
    let py = obj.py();
    let kw = pickler_kwargs(py, protocol, buffer_callback)?;
    module_global(py, intern!(py, "Pickler"))?
        .call((file,), Some(&kw))?
        .call_method1(intern!(py, "dump"), (obj,))?;
    Ok(())
}

/// Serialize obj as a string of bytes allocated in memory
///
/// protocol defaults to cloudpickle.DEFAULT_PROTOCOL which is an alias to
/// pickle.HIGHEST_PROTOCOL. This setting favors maximum communication
/// speed between processes running the same Python version.
///
/// Set protocol=pickle.DEFAULT_PROTOCOL instead if you need to ensure
/// compatibility with older versions of Python (although this is not always
/// guaranteed to work because cloudpickle relies on some internal
/// implementation details that can change from one Python version to the
/// next).
#[pyfunction]
#[pyo3(signature = (obj, protocol=None, buffer_callback=None))]
fn dumps<'py>(
    obj: &Bound<'py, PyAny>,
    protocol: Option<&Bound<'py, PyAny>>,
    buffer_callback: Option<&Bound<'py, PyAny>>,
) -> R<'py> {
    let py = obj.py();
    let g = globals(py);
    // with io.BytesIO() as file:
    let manager = g
        .io_module
        .bind(py)
        .getattr(intern!(py, "BytesIO"))?
        .call0()?;
    let exit = manager.get_type().getattr(intern!(py, "__exit__"))?;
    let file = manager
        .get_type()
        .getattr(intern!(py, "__enter__"))?
        .call1((&manager,))?;
    let body = || -> R<'py> {
        let kw = pickler_kwargs(py, protocol, buffer_callback)?;
        let cp = module_global(py, intern!(py, "Pickler"))?.call((&file,), Some(&kw))?;
        cp.call_method1(intern!(py, "dump"), (obj,))?;
        file.call_method0(intern!(py, "getvalue"))
    };
    match body() {
        Ok(value) => {
            exit.call1((&manager, py.None(), py.None(), py.None()))?;
            Ok(value)
        }
        Err(err) => {
            let suppress =
                exit.call1((&manager, err.get_type(py), err.value(py), err.traceback(py)));
            match suppress {
                Ok(suppress) if suppress.is_truthy()? => Ok(none(py)),
                Ok(_) => Err(err),
                Err(exit_err) => Err(raise_from_context(py, exit_err, err)),
            }
        }
    }
}

/// An exception raised while handling `context` (implicit chaining).
fn raise_from_context(py: Python<'_>, new: PyErr, context: PyErr) -> PyErr {
    let context = context.value(py).clone();
    // SAFETY: PyException_SetContext steals the reference to `context`.
    unsafe { pyo3::ffi::PyException_SetContext(new.value(py).as_ptr(), context.into_ptr()) };
    new
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    add_function(m, wrap_pyfunction!(dump, m)?)?;
    add_function(m, wrap_pyfunction!(dumps, m)?)?;
    Ok(())
}
