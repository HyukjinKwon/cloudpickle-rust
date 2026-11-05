//! Functions called at unpickling time: object reconstructors and state
//! setters, plus a few small public helpers of the upstream module.
//!
//! Their names and signatures are part of the pickle format: payloads
//! reference them as `cloudpickle_rust.cloudpickle.<name>`.

use pyo3::exceptions::{PyDeprecationWarning, PyTypeError, PyValueError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyCFunction, PyDict, PyList, PyModule, PyTuple, PyType};

use crate::fallback::{self, upstream_if_patched};
use crate::lookup::lookup_class_or_track;
use crate::state::globals;
use crate::utils::{add_function, for_each_item, getattr_any, setattr_any, str_eq};

type R<'py> = PyResult<Bound<'py, PyAny>>;

// The `_empty_cell_value` sentinel is created by the module initialization
// (an instance of a Python class, like upstream's).

/// Return whether `func` is a Tornado coroutine function.
///
/// Running coroutines are not supported.
#[pyfunction]
fn is_tornado_coroutine<'py>(func: &Bound<'py, PyAny>) -> R<'py> {
    let py = func.py();
    upstream_if_patched!(py, fallback::IS_TORNADO_COROUTINE, (func,));
    // warnings.warn(..., category=DeprecationWarning): from the frame of the
    // Python function, which attributes the warning to the module, like
    // upstream.
    let kwargs = PyDict::new(py);
    kwargs.set_item(
        intern!(py, "category"),
        py.get_type::<PyDeprecationWarning>(),
    )?;
    fallback::load_global(py, intern!(py, "warnings"))?
        .getattr(intern!(py, "warn"))?
        .call(
            (
                "is_tornado_coroutine is deprecated in cloudpickle 3.0 and will be \
                 removed in cloudpickle 4.0. Use tornado.gen.is_coroutine_function \
                 directly instead.",
            ),
            Some(&kwargs),
        )?;
    let modules = globals(py).sys.bind(py).getattr(intern!(py, "modules"))?;
    if !modules.contains(intern!(py, "tornado.gen"))? {
        return Ok(PyBool::new(py, false).to_owned().into_any());
    }
    let gen = modules.get_item(intern!(py, "tornado.gen"))?;
    if !gen.hasattr(intern!(py, "is_coroutine_function"))? {
        // Tornado version is too old
        return Ok(PyBool::new(py, false).to_owned().into_any());
    }
    gen.call_method1(intern!(py, "is_coroutine_function"), (func,))
}

#[pyfunction]
fn subimport<'py>(name: &Bound<'py, PyAny>) -> R<'py> {
    // We cannot do simply: `return __import__(name)`: Indeed, if ``name`` is
    // the name of a submodule, __import__ will return the top-level root module
    // of this submodule. For instance, __import__('os.path') returns the `os`
    // module.
    let py = name.py();
    upstream_if_patched!(py, fallback::SUBIMPORT, (name,));
    let g = globals(py);
    g.builtins
        .bind(py)
        .getattr(intern!(py, "__import__"))?
        .call1((name,))?;
    g.sys
        .bind(py)
        .getattr(intern!(py, "modules"))?
        .get_item(name)
}

#[pyfunction]
fn dynamic_subimport<'py>(name: &Bound<'py, PyAny>, vars: &Bound<'py, PyAny>) -> R<'py> {
    let py = name.py();
    upstream_if_patched!(py, fallback::DYNAMIC_SUBIMPORT, (name, vars));
    let g = globals(py);
    let module = g.module_type.bind(py).call1((name,))?;
    let module_dict = module.getattr(intern!(py, "__dict__"))?;
    module_dict.call_method1(intern!(py, "update"), (vars,))?;
    module_dict.set_item(intern!(py, "__builtins__"), g.builtins_dict.bind(py))?;
    Ok(module)
}

#[pyfunction]
#[pyo3(name = "_builtin_type")]
fn builtin_type<'py>(name: &Bound<'py, PyAny>) -> R<'py> {
    let py = name.py();
    upstream_if_patched!(py, fallback::BUILTIN_TYPE, (name,));
    if str_eq(name, "ClassType")? {
        // Backward compat to load pickle files generated with cloudpickle
        // < 1.3 even if loading pickle files from older versions is not
        // officially supported.
        return Ok(py.get_type::<PyType>().into_any());
    }
    getattr_any(globals(py).types_module.bind(py).as_any(), name)
}

/// Create a new instance of a class.
///
/// Parameters
/// ----------
/// cls : type
///     The class to create an instance of.
///
/// Returns
/// -------
/// instance : cls
///     A new instance of ``cls``.
#[pyfunction]
fn instance<'py>(cls: &Bound<'py, PyAny>) -> R<'py> {
    cls.call0()
}

#[pyfunction]
#[pyo3(name = "_make_function")]
fn make_function<'py>(
    code: &Bound<'py, PyAny>,
    globals: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
    argdefs: &Bound<'py, PyAny>,
    closure: &Bound<'py, PyAny>,
) -> R<'py> {
    let py = code.py();
    upstream_if_patched!(
        py,
        fallback::MAKE_FUNCTION,
        (code, globals, name, argdefs, closure)
    );
    let g = crate::state::globals(py);
    // Setting __builtins__ in globals is needed for nogil CPython.
    globals.set_item(intern!(py, "__builtins__"), g.builtins_dict.bind(py))?;
    g.function_type
        .bind(py)
        .call1((code, globals, name, argdefs, closure))
}

pub(crate) fn make_empty_cell<'py>(py: Python<'py>) -> R<'py> {
    globals(py).cell_type.bind(py).call0()
}

#[pyfunction]
#[pyo3(name = "_make_empty_cell")]
fn py_make_empty_cell<'py>(py: Python<'py>) -> R<'py> {
    make_empty_cell(py)
}

/// _make_cell(value=_empty_cell_value)
///
/// Create a cell, containing `value` unless it is the `_empty_cell_value`
/// sentinel (which is the default).
//
// The signature cannot be expressed as a `__text_signature__`, which only
// supports literal default values: `inspect.signature` reports no signature,
// like for many builtins.
#[pyfunction]
#[pyo3(name = "_make_cell", signature = (*args, **kwargs), text_signature = None)]
fn make_cell<'py>(
    py: Python<'py>,
    args: &Bound<'py, PyTuple>,
    kwargs: Option<&Bound<'py, PyDict>>,
) -> R<'py> {
    upstream_if_patched!(py, fallback::MAKE_CELL, args, kwargs);
    let mut value = match args.len() {
        0 => None,
        1 => Some(args.get_item(0)?),
        n => {
            return Err(PyTypeError::new_err(format!(
                "_make_cell() takes from 0 to 1 positional arguments but {n} were given"
            )))
        }
    };
    if let Some(kwargs) = kwargs {
        for (key, v) in kwargs.iter() {
            if !str_eq(&key, "value")? {
                return Err(PyTypeError::new_err(format!(
                    "_make_cell() got an unexpected keyword argument '{key}'"
                )));
            }
            if value.is_some() {
                return Err(PyTypeError::new_err(
                    "_make_cell() got multiple values for argument 'value'",
                ));
            }
            value = Some(v);
        }
    }
    let cell = make_empty_cell(py)?;
    if let Some(value) = value {
        if !value.is(globals(py).empty_cell_value.bind(py)) {
            cell.setattr(intern!(py, "cell_contents"), value)?;
        }
    }
    Ok(cell)
}

/// Build dynamic class with an empty __dict__ to be filled once memoized
///
/// If class_tracker_id is not None, try to lookup an existing class definition
/// matching that id. If none is found, track a newly reconstructed class
/// definition under that id so that other instances stemming from the same
/// class id will also reuse this class definition.
///
/// The "extra" variable is meant to be a dict (or None) that can be used for
/// forward compatibility shall the need arise.
#[pyfunction]
#[pyo3(name = "_make_skeleton_class")]
fn make_skeleton_class<'py>(
    type_constructor: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
    bases: &Bound<'py, PyAny>,
    type_kwargs: &Bound<'py, PyAny>,
    class_tracker_id: &Bound<'py, PyAny>,
    extra: &Bound<'py, PyAny>,
) -> R<'py> {
    let py = name.py();
    upstream_if_patched!(
        py,
        fallback::MAKE_SKELETON_CLASS,
        (
            type_constructor,
            name,
            bases,
            type_kwargs,
            class_tracker_id,
            extra
        )
    );
    let g = globals(py);
    // We need to intern the keys of the type_kwargs dict to avoid having
    // different pickles for the same dynamic class depending on whether it was
    // dynamically created or reconstructed from a pickled stream.
    let intern = g.sys.bind(py).getattr(intern!(py, "intern"))?;
    let interned_kwargs = PyDict::new(py);
    for_each_item(type_kwargs, |k, v| {
        interned_kwargs.set_item(intern.call1((k,))?, v)
    })?;

    // `lambda ns: ns.update(type_kwargs)`
    let interned_kwargs = interned_kwargs.unbind();
    let exec_body = PyCFunction::new_closure(
        py,
        None,
        None,
        move |args, _kwargs| -> PyResult<Py<PyAny>> {
            let py = args.py();
            let ns = args.get_item(0)?;
            Ok(ns
                .call_method1(intern!(py, "update"), (interned_kwargs.bind(py),))?
                .unbind())
        },
    )?;
    let kwds = PyDict::new(py);
    kwds.set_item(intern!(py, "metaclass"), type_constructor)?;
    let new_class = g.types_module.bind(py).getattr(intern!(py, "new_class"))?;
    let skeleton_class = new_class.call1((name, bases, kwds, exec_body))?;

    lookup_class_or_track(class_tracker_id, &skeleton_class)
}

/// Build dynamic enum with an empty __dict__ to be filled once memoized
///
/// The creation of the enum class is inspired by the code of
/// EnumMeta._create_.
///
/// If class_tracker_id is not None, try to lookup an existing enum definition
/// matching that id. If none is found, track a newly reconstructed enum
/// definition under that id so that other instances stemming from the same
/// class id will also reuse this enum definition.
///
/// The "extra" variable is meant to be a dict (or None) that can be used for
/// forward compatibility shall the need arise.
#[pyfunction]
#[pyo3(name = "_make_skeleton_enum")]
#[allow(clippy::too_many_arguments)]
fn make_skeleton_enum<'py>(
    bases: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
    qualname: &Bound<'py, PyAny>,
    members: &Bound<'py, PyAny>,
    module: &Bound<'py, PyAny>,
    class_tracker_id: &Bound<'py, PyAny>,
    extra: &Bound<'py, PyAny>,
) -> R<'py> {
    let py = name.py();
    upstream_if_patched!(
        py,
        fallback::MAKE_SKELETON_ENUM,
        (
            bases,
            name,
            qualname,
            members,
            module,
            class_tracker_id,
            extra
        )
    );
    // enums always inherit from their base Enum class at the last position in
    // the list of base classes:
    let enum_base = bases.get_item(-1)?;
    let metacls = enum_base.getattr(intern!(py, "__class__"))?;
    let classdict = metacls.call_method1(intern!(py, "__prepare__"), (name, bases))?;

    for_each_item(members, |member_name, member_value| {
        classdict.set_item(member_name, member_value)
    })?;
    let enum_class =
        metacls.call_method1(intern!(py, "__new__"), (&metacls, name, bases, classdict))?;
    enum_class.setattr(intern!(py, "__module__"), module)?;
    enum_class.setattr(intern!(py, "__qualname__"), qualname)?;

    lookup_class_or_track(class_tracker_id, &enum_class)
}

#[pyfunction]
#[pyo3(name = "_make_typevar")]
fn make_typevar<'py>(
    name: &Bound<'py, PyAny>,
    bound: &Bound<'py, PyAny>,
    constraints: &Bound<'py, PyAny>,
    covariant: &Bound<'py, PyAny>,
    contravariant: &Bound<'py, PyAny>,
    class_tracker_id: &Bound<'py, PyAny>,
) -> R<'py> {
    let py = name.py();
    upstream_if_patched!(
        py,
        fallback::MAKE_TYPEVAR,
        (
            name,
            bound,
            constraints,
            covariant,
            contravariant,
            class_tracker_id
        )
    );
    let mut args = vec![name.clone()];
    for constraint in constraints.try_iter()? {
        args.push(constraint?);
    }
    let kwargs = PyDict::new(py);
    kwargs.set_item(intern!(py, "bound"), bound)?;
    kwargs.set_item(intern!(py, "covariant"), covariant)?;
    kwargs.set_item(intern!(py, "contravariant"), contravariant)?;
    let typevar = globals(py)
        .typing_module
        .bind(py)
        .getattr(intern!(py, "TypeVar"))?;
    let tv = typevar.call(PyTuple::new(py, args)?, Some(&kwargs))?;
    lookup_class_or_track(class_tracker_id, &tv)
}

#[pyfunction]
#[pyo3(name = "_make_dict_keys", signature = (obj, is_ordered=None), text_signature = "(obj, is_ordered=False)")]
fn make_dict_keys<'py>(obj: &Bound<'py, PyAny>, is_ordered: Option<&Bound<'py, PyAny>>) -> R<'py> {
    let py = obj.py();
    if !fallback::MAKE_DICT_KEYS.pristine(py)? {
        return match is_ordered {
            Some(is_ordered) => fallback::MAKE_DICT_KEYS.call(py, (obj, is_ordered), None),
            None => fallback::MAKE_DICT_KEYS.call(py, (obj,), None),
        };
    }
    let factory = if is_truthy_opt(is_ordered)? {
        globals(py).ordered_dict.bind(py).clone()
    } else {
        py.get_type::<PyDict>().into_any()
    };
    factory
        .call_method1(intern!(py, "fromkeys"), (obj,))?
        .call_method0(intern!(py, "keys"))
}

#[pyfunction]
#[pyo3(name = "_make_dict_values", signature = (obj, is_ordered=None), text_signature = "(obj, is_ordered=False)")]
fn make_dict_values<'py>(
    obj: &Bound<'py, PyAny>,
    is_ordered: Option<&Bound<'py, PyAny>>,
) -> R<'py> {
    let py = obj.py();
    if !fallback::MAKE_DICT_VALUES.pristine(py)? {
        return match is_ordered {
            Some(is_ordered) => fallback::MAKE_DICT_VALUES.call(py, (obj, is_ordered), None),
            None => fallback::MAKE_DICT_VALUES.call(py, (obj,), None),
        };
    }
    if is_truthy_opt(is_ordered)? {
        let pairs = PyList::empty(py);
        for (i, value) in obj.try_iter()?.enumerate() {
            pairs.append((i, value?))?;
        }
        globals(py)
            .ordered_dict
            .bind(py)
            .call1((pairs,))?
            .call_method0(intern!(py, "values"))
    } else {
        let d = PyDict::new(py);
        for (i, value) in obj.try_iter()?.enumerate() {
            d.set_item(i, value?)?;
        }
        d.call_method0(intern!(py, "values"))
    }
}

#[pyfunction]
#[pyo3(name = "_make_dict_items", signature = (obj, is_ordered=None), text_signature = "(obj, is_ordered=False)")]
fn make_dict_items<'py>(obj: &Bound<'py, PyAny>, is_ordered: Option<&Bound<'py, PyAny>>) -> R<'py> {
    let py = obj.py();
    if !fallback::MAKE_DICT_ITEMS.pristine(py)? {
        return match is_ordered {
            Some(is_ordered) => fallback::MAKE_DICT_ITEMS.call(py, (obj, is_ordered), None),
            None => fallback::MAKE_DICT_ITEMS.call(py, (obj,), None),
        };
    }
    if is_truthy_opt(is_ordered)? {
        globals(py)
            .ordered_dict
            .bind(py)
            .call1((obj,))?
            .call_method0(intern!(py, "items"))
    } else {
        obj.call_method0(intern!(py, "items"))
    }
}

/// `bool(value)` for an optional argument defaulting to a falsy value.
fn is_truthy_opt(value: Option<&Bound<'_, PyAny>>) -> PyResult<bool> {
    value.map_or(Ok(false), |v| v.is_truthy())
}

#[pyfunction]
#[pyo3(name = "_file_reconstructor")]
fn file_reconstructor<'py>(retval: &Bound<'py, PyAny>) -> R<'py> {
    Ok(retval.clone())
}

/// Update the state of a dynamic function.
///
/// As __closure__ and __globals__ are readonly attributes of a function, we
/// cannot rely on the native setstate routine of pickle.load_build, that calls
/// setattr on items of the slotstate. Instead, we have to modify them inplace.
#[pyfunction]
#[pyo3(name = "_function_setstate")]
fn function_setstate<'py>(obj: &Bound<'py, PyAny>, state: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::FUNCTION_SETSTATE, (obj, state));
    let g = globals(py);
    let [state, slotstate]: [Bound<'py, PyAny>; 2] =
        fallback::unpack(state, 2)?.try_into().expect("two values");
    obj.getattr(intern!(py, "__dict__"))?
        .call_method1(intern!(py, "update"), (state,))?;

    let obj_globals = slotstate.call_method1(intern!(py, "pop"), (intern!(py, "__globals__"),))?;
    let obj_closure = slotstate.call_method1(intern!(py, "pop"), (intern!(py, "__closure__"),))?;
    // _cloudpickle_subimports is a set of submodules that must be loaded for
    // the pickled function to work correctly at unpickling time. Now that these
    // submodules are depickled (hence imported), they can be removed from the
    // object's state (the object state only served as a reference holder to
    // these submodules)
    slotstate.call_method1(
        intern!(py, "pop"),
        (intern!(py, "_cloudpickle_submodules"),),
    )?;

    let func_globals = obj.getattr(intern!(py, "__globals__"))?;
    func_globals.call_method1(intern!(py, "update"), (obj_globals,))?;
    func_globals.set_item(intern!(py, "__builtins__"), g.builtins_dict.bind(py))?;

    if !obj_closure.is_none() {
        for (i, cell) in obj_closure.try_iter()?.enumerate() {
            let value = match cell?.getattr(intern!(py, "cell_contents")) {
                Ok(value) => value,
                // cell is empty
                Err(err) if err.is_instance_of::<PyValueError>(py) => continue,
                Err(err) => return Err(err),
            };
            obj.getattr(intern!(py, "__closure__"))?
                .get_item(i)?
                .setattr(intern!(py, "cell_contents"), value)?;
        }
    }

    for_each_item(&slotstate, |k, v| setattr_any(obj, &k, &v))?;
    Ok(py.None().into_bound(py))
}

#[pyfunction]
#[pyo3(name = "_class_setstate")]
fn class_setstate<'py>(obj: &Bound<'py, PyAny>, state: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::CLASS_SETSTATE, (obj, state));
    let g = globals(py);
    let [state, _slotstate]: [Bound<'py, PyAny>; 2] =
        fallback::unpack(state, 2)?.try_into().expect("two values");
    let mut registry: Option<Bound<'py, PyAny>> = None;
    for_each_item(&state, |attrname, attr| {
        if attrname.eq(intern!(py, "_abc_impl"))? {
            registry = Some(attr);
        } else {
            // Note: setting attribute names on a class automatically triggers their
            // interning in CPython:
            // https://github.com/python/cpython/blob/v3.12.0/Objects/object.c#L957
            //
            // This means that to get deterministic pickling for a dynamic class that
            // was initially defined in a different Python process, the pickler
            // needs to ensure that dynamic class and function attribute names are
            // systematically copied into a non-interned version to avoid
            // unpredictable pickle payloads.
            //
            // Indeed the Pickler's memoizer relies on physical object identity to break
            // cycles in the reference graph of the object being serialized.
            setattr_any(obj, &attrname, &attr)?;
        }
        Ok(())
    })?;

    if g.at_least(3, 13) && state.contains(intern!(py, "__firstlineno__"))? {
        // Set the Python 3.13+ only __firstlineno__  attribute one more time, as it
        // will be automatically deleted by the `setattr(obj, attrname, attr)` call
        // above when `attrname` is "__firstlineno__". We assume that preserving this
        // information might be important for some users and that it not stale in the
        // context of cloudpickle usage, hence legitimate to propagate. Furthermore it
        // is necessary to do so to keep deterministic chained pickling as tested in
        // test_deterministic_str_interning_for_chained_dynamic_class_pickling.
        obj.setattr(
            intern!(py, "__firstlineno__"),
            state.get_item(intern!(py, "__firstlineno__"))?,
        )?;
    }

    if let Some(registry) = registry.filter(|r| !r.is_none()) {
        for subclass in registry.try_iter()? {
            obj.call_method1(intern!(py, "register"), (subclass?,))?;
        }
    }

    // PEP-649/749: During pickling, we excluded the __annotate_func__ attribute but it
    // will be created by Python. Subsequently, annotations will be recreated when
    // __annotations__ is accessed.

    Ok(obj.clone())
}

#[pyfunction]
#[pyo3(name = "_get_dataclass_field_type_sentinel")]
fn get_dataclass_field_type_sentinel<'py>(name: &Bound<'py, PyAny>) -> R<'py> {
    let py = name.py();
    upstream_if_patched!(py, fallback::GET_DATACLASS_FIELD_TYPE_SENTINEL, (name,));
    globals(py)
        .dataclass_sentinels
        .bind(py)
        .as_any()
        .get_item(name)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    add_function(m, wrap_pyfunction!(builtin_type, m)?)?;
    add_function(m, wrap_pyfunction!(is_tornado_coroutine, m)?)?;
    add_function(m, wrap_pyfunction!(subimport, m)?)?;
    add_function(m, wrap_pyfunction!(dynamic_subimport, m)?)?;
    add_function(m, wrap_pyfunction!(instance, m)?)?;
    add_function(m, wrap_pyfunction!(make_function, m)?)?;
    add_function(m, wrap_pyfunction!(py_make_empty_cell, m)?)?;
    add_function(m, wrap_pyfunction!(make_cell, m)?)?;
    add_function(m, wrap_pyfunction!(make_skeleton_class, m)?)?;
    add_function(m, wrap_pyfunction!(make_skeleton_enum, m)?)?;
    add_function(m, wrap_pyfunction!(make_typevar, m)?)?;
    add_function(m, wrap_pyfunction!(make_dict_keys, m)?)?;
    add_function(m, wrap_pyfunction!(make_dict_values, m)?)?;
    add_function(m, wrap_pyfunction!(make_dict_items, m)?)?;
    add_function(m, wrap_pyfunction!(file_reconstructor, m)?)?;
    add_function(m, wrap_pyfunction!(function_setstate, m)?)?;
    add_function(m, wrap_pyfunction!(class_setstate, m)?)?;
    add_function(m, wrap_pyfunction!(get_dataclass_field_type_sentinel, m)?)?;
    Ok(())
}
