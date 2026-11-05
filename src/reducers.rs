//! Reducers and state getters.
//!
//! A reducer is a function taking a single argument (obj), and that returns a
//! tuple with all the necessary data to re-construct obj. Apart from a few
//! exceptions (list, dict, bytes, int, etc.), a reducer is necessary to
//! correctly pickle an object.
//! While many built-in objects (Exceptions objects, instances of the "object"
//! class, etc), are shipped with their own built-in reducer (invoked using
//! obj.__reduce__), some do not. The following methods were created to "fill
//! these holes".

use pyo3::exceptions::PyValueError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule, PyString, PyTuple};

use crate::code::{
    extract_code_globals, find_imported_submodules, find_imported_submodules_native, is_code,
};
use crate::fallback::{self, upstream_if_patched};
use crate::lookup::{
    get_or_create_tracker_id, is_registered_pickle_by_value, lookup_module_and_qualname,
    should_pickle_by_reference,
};
use crate::state::globals;
use crate::utils::{
    add_function, dict_pop, dict_pop_required, for_each_item, fresh_str, is_subclass, raise_from,
};

type R<'py> = PyResult<Bound<'py, PyAny>>;

fn not_implemented(py: Python<'_>) -> Bound<'_, PyAny> {
    py.NotImplemented().into_bound(py)
}

// ---------------------------------------------------------------------------
// Classes
// ---------------------------------------------------------------------------

/// Retrieve a copy of the dict of a class without the inherited method.
pub(crate) fn extract_class_dict<'py>(cls: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let py = cls.py();
    let cls_dict = cls.getattr(intern!(py, "__dict__"))?;
    // Hack to circumvent non-predictable memoization caused by string interning.
    // See the inline comment in _class_setstate for details.
    let keys = py
        .get_type::<PyList>()
        .call1((&cls_dict,))?
        .cast_into::<PyList>()?;
    keys.sort()?;
    let clsdict = PyDict::new(py);
    for k in keys.iter() {
        let value = cls_dict.get_item(&k)?;
        clsdict.set_item(fresh_str(&k)?, value)?;
    }

    let bases = cls.getattr(intern!(py, "__bases__"))?;
    let inherited_dict = if bases.len()? == 1 {
        bases.get_item(0)?.getattr(intern!(py, "__dict__"))?
    } else {
        let inherited = PyDict::new(py);
        let reversed = globals(py)
            .builtins
            .bind(py)
            .getattr(intern!(py, "reversed"))?;
        for base in reversed.call1((&bases,))?.try_iter()? {
            inherited.call_method1(
                intern!(py, "update"),
                (base?.getattr(intern!(py, "__dict__"))?,),
            )?;
        }
        inherited.into_any()
    };
    let mut to_remove = Vec::new();
    for (name, value) in clsdict.iter() {
        match inherited_dict.get_item(&name) {
            Ok(base_value) => {
                if value.is(&base_value) {
                    to_remove.push(name);
                }
            }
            Err(err) if err.is_instance_of::<pyo3::exceptions::PyKeyError>(py) => {}
            Err(err) => return Err(err),
        }
    }
    for name in to_remove {
        dict_pop_required(&clsdict, &name)?;
    }
    Ok(clsdict)
}

pub(crate) fn get_bases<'py>(typ: &Bound<'py, PyAny>) -> R<'py> {
    let py = typ.py();
    let has_orig_bases = match typ.getattr_opt(intern!(py, "__dict__"))? {
        Some(d) => d.contains(intern!(py, "__orig_bases__"))?,
        None => false,
    };
    if has_orig_bases {
        // For generic types (see PEP 560)
        // Note that simply checking `hasattr(typ, '__orig_bases__')` is not
        // correct.  Subclasses of a fully-parameterized generic class does not
        // have `__orig_bases__` defined, but `hasattr(typ, '__orig_bases__')`
        // will return True because it's defined in the base class.
        typ.getattr(intern!(py, "__orig_bases__"))
    } else {
        // For regular class objects
        typ.getattr(intern!(py, "__bases__"))
    }
}

pub(crate) fn class_getnewargs<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::CLASS_GETNEWARGS, (obj,));
    let g = globals(py);
    let type_kwargs = PyDict::new(py);
    let obj_dict = obj.getattr(intern!(py, "__dict__"))?;
    if obj_dict.contains(intern!(py, "__module__"))? {
        type_kwargs.set_item(
            intern!(py, "__module__"),
            obj.getattr(intern!(py, "__module__"))?,
        )?;
    }

    let dict_attr =
        obj_dict.call_method1(intern!(py, "get"), (intern!(py, "__dict__"), py.None()))?;
    if dict_attr.is_instance(g.property_type.bind(py))? {
        type_kwargs.set_item(intern!(py, "__dict__"), dict_attr)?;
    }

    Ok((
        obj.get_type(),
        obj.getattr(intern!(py, "__name__"))?,
        get_bases(obj)?,
        type_kwargs,
        get_or_create_tracker_id(obj)?,
        py.None(),
    )
        .into_pyobject(py)?
        .into_any())
}

/// `{e.name: e.value for e in obj}` for an Enum class.
fn enum_members<'py>(obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let py = obj.py();
    let members = PyDict::new(py);
    for e in obj.try_iter()? {
        let e = e?;
        members.set_item(
            e.getattr(intern!(py, "name"))?,
            e.getattr(intern!(py, "value"))?,
        )?;
    }
    Ok(members)
}

pub(crate) fn enum_getnewargs<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::ENUM_GETNEWARGS, (obj,));
    let members = enum_members(obj)?;
    Ok((
        obj.getattr(intern!(py, "__bases__"))?,
        obj.getattr(intern!(py, "__name__"))?,
        obj.getattr(intern!(py, "__qualname__"))?,
        members,
        obj.getattr(intern!(py, "__module__"))?,
        get_or_create_tracker_id(obj)?,
        py.None(),
    )
        .into_pyobject(py)?
        .into_any())
}

fn class_getstate_parts<'py>(
    obj: &Bound<'py, PyAny>,
) -> PyResult<(Bound<'py, PyDict>, Bound<'py, PyDict>)> {
    let py = obj.py();
    let g = globals(py);
    let clsdict = extract_class_dict(obj)?;
    dict_pop(&clsdict, intern!(py, "__weakref__"))?;

    if is_subclass(
        &obj.get_type(),
        &g.abc_module.bind(py).getattr(intern!(py, "ABCMeta"))?,
    )? {
        // If obj is an instance of an ABCMeta subclass, don't pickle the
        // cache/negative caches populated during isinstance/issubclass
        // checks, but pickle the list of registered subclasses of obj.
        dict_pop(&clsdict, intern!(py, "_abc_cache"))?;
        dict_pop(&clsdict, intern!(py, "_abc_negative_cache"))?;
        dict_pop(&clsdict, intern!(py, "_abc_negative_cache_version"))?;
        let registry = dict_pop(&clsdict, intern!(py, "_abc_registry"))?.filter(|r| !r.is_none());
        let abc_impl = PyList::empty(py);
        match registry {
            None => {
                // The abc caches and registered subclasses of a
                // class are bundled into the single _abc_impl attribute
                dict_pop(&clsdict, intern!(py, "_abc_impl"))?;
                let dump = g
                    .abc_module
                    .bind(py)
                    .call_method1(intern!(py, "_get_dump"), (obj,))?;
                // (registry, _, _, _) = abc._get_dump(obj)
                let values = fallback::unpack(&dump, 4)?;
                let registry = &values[0];
                for subclass_weakref in registry.try_iter()? {
                    abc_impl.append(subclass_weakref?.call0()?)?;
                }
            }
            Some(registry) => {
                // In the above if clause, registry is a set of weakrefs -- in
                // this case, registry is a WeakSet
                for type_ in registry.try_iter()? {
                    abc_impl.append(type_?)?;
                }
            }
        }
        clsdict.set_item(intern!(py, "_abc_impl"), abc_impl)?;
    }

    if clsdict.contains(intern!(py, "__slots__"))? {
        // pickle string length optimization: member descriptors of obj are
        // created automatically from obj's __slots__ attribute, no need to
        // save them in obj's state
        let slots = obj.getattr(intern!(py, "__slots__"))?;
        if slots.is_instance(g.str_type.bind(py))? {
            dict_pop_required(&clsdict, &slots)?;
        } else {
            for k in slots.try_iter()? {
                dict_pop(&clsdict, &k?)?;
            }
        }
    }

    dict_pop(&clsdict, intern!(py, "__dict__"))?; // unpicklable property object

    if g.at_least(3, 14) {
        // PEP-649/749: __annotate_func__ contains a closure that references the class
        // dict. We need to exclude it from pickling. Python will recreate it when
        // __annotations__ is accessed at unpickling time.
        dict_pop(&clsdict, intern!(py, "__annotate_func__"))?;
    }

    Ok((clsdict, PyDict::new(py)))
}

pub(crate) fn class_getstate<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::CLASS_GETSTATE, (obj,));
    Ok(class_getstate_parts(obj)?.into_pyobject(py)?.into_any())
}

pub(crate) fn enum_getstate<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::ENUM_GETSTATE, (obj,));
    let (clsdict, slotstate) = class_getstate_parts(obj)?;

    let members = enum_members(obj)?;
    // Cleanup the clsdict that will be passed to _make_skeleton_enum:
    // Those attributes are already handled by the metaclass.
    for attrname in [
        intern!(py, "_generate_next_value_"),
        intern!(py, "_member_names_"),
        intern!(py, "_member_map_"),
        intern!(py, "_member_type_"),
        intern!(py, "_value2member_map_"),
    ] {
        dict_pop(&clsdict, attrname)?;
    }
    for member in members.keys() {
        dict_pop_required(&clsdict, &member)?;
    }
    // Special handling of Enum subclasses
    Ok((clsdict, slotstate).into_pyobject(py)?.into_any())
}

/// Save a class that can't be referenced as a module attribute.
///
/// This method is used to serialize classes that are defined inside
/// functions, or that otherwise can't be serialized as attribute lookups
/// from importable modules.
pub(crate) fn dynamic_class_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::DYNAMIC_CLASS_REDUCE, (obj,));
    let g = globals(py);
    let reduced = if is_subclass(obj, g.enum_type.bind(py))? {
        (
            g.make_skeleton_enum.bind(py),
            enum_getnewargs(obj)?,
            enum_getstate(obj)?,
            py.None(),
            py.None(),
            g.class_setstate.bind(py),
        )
    } else {
        (
            g.make_skeleton_class.bind(py),
            class_getnewargs(obj)?,
            class_getstate(obj)?,
            py.None(),
            py.None(),
            g.class_setstate.bind(py),
        )
    };
    Ok(reduced.into_pyobject(py)?.into_any())
}

/// Select the reducer depending on the dynamic nature of the class obj.
pub(crate) fn class_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::CLASS_REDUCE, (obj,));
    let g = globals(py);
    let type_type = py.get_type::<pyo3::types::PyType>();
    if obj.is(g.none_type.bind(py)) {
        return Ok((type_type, (py.None(),)).into_pyobject(py)?.into_any());
    }
    if obj.is(g.ellipsis_type.bind(py)) {
        return Ok((type_type, (py.Ellipsis(),)).into_pyobject(py)?.into_any());
    }
    if obj.is(g.notimplemented_type.bind(py)) {
        return Ok((type_type, (py.NotImplemented(),))
            .into_pyobject(py)?
            .into_any());
    }
    if let Some(name) = g.builtin_type_names.bind(py).get_item(obj)? {
        return Ok((g.builtin_type.bind(py), (name,))
            .into_pyobject(py)?
            .into_any());
    }
    if !should_pickle_by_reference(obj, None)?.is_truthy()? {
        return dynamic_class_reduce(obj);
    }
    Ok(not_implemented(py))
}

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

pub(crate) fn get_cell_contents<'py>(cell: &Bound<'py, PyAny>) -> R<'py> {
    let py = cell.py();
    upstream_if_patched!(py, fallback::GET_CELL_CONTENTS, (cell,));
    match cell.getattr(intern!(py, "cell_contents")) {
        Ok(value) => Ok(value),
        // Handle empty cells explicitly with a sentinel value.
        Err(err) if err.is_instance_of::<PyValueError>(py) => {
            Ok(globals(py).empty_cell_value.bind(py).clone())
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn function_getstate<'py>(func: &Bound<'py, PyAny>) -> R<'py> {
    // - Put func's dynamic attributes (stored in func.__dict__) in state. These
    //   attributes will be restored at unpickling time using
    //   f.__dict__.update(state)
    // - Put func's members into slotstate. Such attributes will be restored at
    //   unpickling time by iterating over slotstate and calling setattr(func,
    //   slotname, slotvalue)
    let py = func.py();
    upstream_if_patched!(py, fallback::FUNCTION_GETSTATE, (func,));
    let slotstate = PyDict::new(py);
    // Hack to circumvent non-predictable memoization caused by string interning.
    // See the inline comment in _class_setstate for details.
    slotstate.set_item(
        intern!(py, "__name__"),
        fresh_str(&func.getattr(intern!(py, "__name__"))?)?,
    )?;
    slotstate.set_item(
        intern!(py, "__qualname__"),
        fresh_str(&func.getattr(intern!(py, "__qualname__"))?)?,
    )?;
    for attr in [
        intern!(py, "__annotations__"),
        intern!(py, "__kwdefaults__"),
        intern!(py, "__defaults__"),
        intern!(py, "__module__"),
        intern!(py, "__doc__"),
        intern!(py, "__closure__"),
    ] {
        slotstate.set_item(attr, func.getattr(attr)?)?;
    }

    let code = func.getattr(intern!(py, "__code__"))?;
    let f_globals_ref = extract_code_globals(&code)?;
    let func_globals = func.getattr(intern!(py, "__globals__"))?;
    let f_globals = PyDict::new(py);
    match (
        f_globals_ref.cast_exact::<PyDict>(),
        func_globals.cast_exact::<PyDict>(),
    ) {
        (Ok(names), Ok(func_globals)) => {
            for k in names.keys() {
                if let Some(v) = func_globals.get_item(&k)? {
                    f_globals.set_item(k, v)?;
                }
            }
        }
        _ => {
            for k in f_globals_ref.try_iter()? {
                let k = k?;
                if func_globals.contains(&k)? {
                    f_globals.set_item(&k, func_globals.get_item(&k)?)?;
                }
            }
        }
    }

    let closure = func.getattr(intern!(py, "__closure__"))?;
    let closure_values = if closure.is_none() {
        PyTuple::empty(py).into_any()
    } else {
        let values = PyList::empty(py);
        for cell in closure.try_iter()? {
            values.append(get_cell_contents(&cell?)?)?;
        }
        values.into_any()
    };

    // Extract currently-imported submodules used by func. Storing these modules
    // in a smoke _cloudpickle_subimports attribute of the object's state will
    // trigger the side effect of importing these modules at unpickling time
    // (which is necessary for func to work correctly once depickled)
    let submodules = if is_code(&code) && fallback::FIND_IMPORTED_SUBMODULES.pristine(py)? {
        // The Rust implementation iterates over the dependencies like upstream's
        // over `itertools.chain(...)`.
        let closure_values = closure_values.try_iter()?.collect::<PyResult<Vec<_>>>()?;
        let dependencies = f_globals.values().into_iter().chain(closure_values).map(Ok);
        find_imported_submodules_native(&code, dependencies)?
    } else {
        let dependencies = fallback::load_global(py, intern!(py, "itertools"))?
            .getattr(intern!(py, "chain"))?
            .call1((
                f_globals.call_method0(intern!(py, "values"))?,
                closure_values,
            ))?;
        find_imported_submodules(&code, &dependencies)?
    };
    slotstate.set_item(intern!(py, "_cloudpickle_submodules"), submodules)?;
    slotstate.set_item(intern!(py, "__globals__"), f_globals)?;

    // Hack to circumvent non-predictable memoization caused by string interning.
    // See the inline comment in _class_setstate for details.
    let state = PyDict::new(py);
    for_each_item(&func.getattr(intern!(py, "__dict__"))?, |k, v| {
        state.set_item(fresh_str(&k)?, v)
    })?;
    Ok((state, slotstate).into_pyobject(py)?.into_any())
}

// ---------------------------------------------------------------------------
// Other reducers (the values of `Pickler._dispatch_table`)
// ---------------------------------------------------------------------------

/// Cell (containing values of a function's free variables) reducer.
fn cell_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::CELL_REDUCE, (obj,));
    let g = globals(py);
    match obj.getattr(intern!(py, "cell_contents")) {
        // cell is empty
        Err(err) if err.is_instance_of::<PyValueError>(py) => {
            Ok((g.make_empty_cell.bind(py), PyTuple::empty(py))
                .into_pyobject(py)?
                .into_any())
        }
        Err(err) => Err(err),
        Ok(contents) => Ok((g.make_cell.bind(py), (contents,))
            .into_pyobject(py)?
            .into_any()),
    }
}

fn classmethod_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    let orig_func = obj.getattr(intern!(py, "__func__"))?;
    Ok((obj.get_type(), (orig_func,)).into_pyobject(py)?.into_any())
}

fn pickling_error(py: Python<'_>, msg: String) -> PyErr {
    PyErr::from_type(globals(py).pickling_error.bind(py).clone(), msg)
}

/// `pickle.PicklingError(format % value)`.
fn pickling_error_formatted(py: Python<'_>, format: &str, value: &Bound<'_, PyAny>) -> PyErr {
    match PyString::new(py, format).rem(value) {
        Ok(msg) => PyErr::from_type(globals(py).pickling_error.bind(py).clone(), (msg.unbind(),)),
        Err(err) => err,
    }
}

/// Save a file.
fn file_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::FILE_REDUCE, (obj,));
    let g = globals(py);
    let sys = g.sys.bind(py);
    if !obj.hasattr(intern!(py, "name"))? || !obj.hasattr(intern!(py, "mode"))? {
        return Err(pickling_error(
            py,
            "Cannot pickle files that do not map to an actual file".into(),
        ));
    }
    if obj.is(&sys.getattr(intern!(py, "stdout"))?) {
        return Ok((g.getattr.bind(py), (sys, intern!(py, "stdout")))
            .into_pyobject(py)?
            .into_any());
    }
    if obj.is(&sys.getattr(intern!(py, "stderr"))?) {
        return Ok((g.getattr.bind(py), (sys, intern!(py, "stderr")))
            .into_pyobject(py)?
            .into_any());
    }
    if obj.is(&sys.getattr(intern!(py, "stdin"))?) {
        return Err(pickling_error(py, "Cannot pickle standard input".into()));
    }
    if obj.getattr(intern!(py, "closed"))?.is_truthy()? {
        return Err(pickling_error(py, "Cannot pickle closed files".into()));
    }
    if obj.hasattr(intern!(py, "isatty"))?
        && obj.call_method0(intern!(py, "isatty"))?.is_truthy()?
    {
        return Err(pickling_error(
            py,
            "Cannot pickle files that map to tty objects".into(),
        ));
    }
    let mode = obj.getattr(intern!(py, "mode"))?;
    if !mode.contains("r")? && !mode.contains("+")? {
        return Err(pickling_error_formatted(
            py,
            "Cannot pickle files that are not opened for reading: %s",
            &mode,
        ));
    }

    let name = obj.getattr(intern!(py, "name"))?;

    // `import io` in the body of upstream's function.
    let retval = py.import("io")?.getattr(intern!(py, "StringIO"))?.call0()?;

    let read = || -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        // Read the whole file
        let curloc = obj.call_method0(intern!(py, "tell"))?;
        obj.call_method1(intern!(py, "seek"), (0,))?;
        let contents = obj.call_method0(intern!(py, "read"))?;
        obj.call_method1(intern!(py, "seek"), (&curloc,))?;
        Ok((curloc, contents))
    };
    let (curloc, contents) = match read() {
        Ok(v) => v,
        Err(err) if err.is_instance_of::<pyo3::exceptions::PyOSError>(py) => {
            let new =
                pickling_error_formatted(py, "Cannot pickle file %s as it cannot be read", &name);
            return Err(raise_from(py, new, err));
        }
        Err(err) => return Err(err),
    };
    retval.call_method1(intern!(py, "write"), (contents,))?;
    retval.call_method1(intern!(py, "seek"), (curloc,))?;

    retval.setattr(intern!(py, "name"), name)?;
    Ok((g.file_reconstructor.bind(py), (retval,))
        .into_pyobject(py)?
        .into_any())
}

fn getset_descriptor_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    let g = globals(py);
    Ok((
        g.getattr.bind(py),
        (
            obj.getattr(intern!(py, "__objclass__"))?,
            obj.getattr(intern!(py, "__name__"))?,
        ),
    )
        .into_pyobject(py)?
        .into_any())
}

fn mappingproxy_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::MAPPINGPROXY_REDUCE, (obj,));
    let g = globals(py);
    let as_dict = py.get_type::<PyDict>().call1((obj,))?;
    let mappingproxy_type = g
        .types_module
        .bind(py)
        .getattr(intern!(py, "MappingProxyType"))?;
    Ok((mappingproxy_type, (as_dict,))
        .into_pyobject(py)?
        .into_any())
}

fn memoryview_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    let g = globals(py);
    let data = obj.call_method0(intern!(py, "tobytes"))?;
    Ok((g.bytes_type.bind(py), (data,))
        .into_pyobject(py)?
        .into_any())
}

fn module_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::MODULE_REDUCE, (obj,));
    let g = globals(py);
    if should_pickle_by_reference(obj, None)?.is_truthy()? {
        Ok((
            g.subimport.bind(py),
            (obj.getattr(intern!(py, "__name__"))?,),
        )
            .into_pyobject(py)?
            .into_any())
    } else {
        // Some external libraries can populate the "__builtins__" entry of a
        // module's `__dict__` with unpicklable objects (see #316). For that
        // reason, we do not attempt to pickle the "__builtins__" entry, and
        // restore a default value for it at unpickling time.
        let state = obj
            .getattr(intern!(py, "__dict__"))?
            .call_method0(intern!(py, "copy"))?;
        state.call_method1(intern!(py, "pop"), (intern!(py, "__builtins__"), py.None()))?;
        Ok((
            g.dynamic_subimport.bind(py),
            (obj.getattr(intern!(py, "__name__"))?, state),
        )
            .into_pyobject(py)?
            .into_any())
    }
}

fn method_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::METHOD_REDUCE, (obj,));
    let g = globals(py);
    Ok((
        g.types_module.bind(py).getattr(intern!(py, "MethodType"))?,
        (
            obj.getattr(intern!(py, "__func__"))?,
            obj.getattr(intern!(py, "__self__"))?,
        ),
    )
        .into_pyobject(py)?
        .into_any())
}

fn logger_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::LOGGER_REDUCE, (obj,));
    let g = globals(py);
    let get_logger = g
        .logging_module
        .bind(py)
        .getattr(intern!(py, "getLogger"))?;
    Ok((get_logger, (obj.getattr(intern!(py, "name"))?,))
        .into_pyobject(py)?
        .into_any())
}

fn root_logger_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::ROOT_LOGGER_REDUCE, (obj,));
    let g = globals(py);
    let get_logger = g
        .logging_module
        .bind(py)
        .getattr(intern!(py, "getLogger"))?;
    Ok((get_logger, PyTuple::empty(py))
        .into_pyobject(py)?
        .into_any())
}

fn property_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    let g = globals(py);
    Ok((
        g.property_type.bind(py),
        (
            obj.getattr(intern!(py, "fget"))?,
            obj.getattr(intern!(py, "fset"))?,
            obj.getattr(intern!(py, "fdel"))?,
            obj.getattr(intern!(py, "__doc__"))?,
        ),
    )
        .into_pyobject(py)?
        .into_any())
}

fn weakset_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::WEAKSET_REDUCE, (obj,));
    let g = globals(py);
    let items = py.get_type::<PyList>().call1((obj,))?;
    let weakset = g.weakref_module.bind(py).getattr(intern!(py, "WeakSet"))?;
    Ok((weakset, (items,)).into_pyobject(py)?.into_any())
}

fn as_list<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    obj.py().get_type::<PyList>().call1((obj,))
}

fn as_dict<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    obj.py().get_type::<PyDict>().call1((obj,))
}

// Safer not to ship the full dict as sending the rest might
// be unintended and could potentially cause leaking of
// sensitive information
fn dict_keys_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::DICT_KEYS_REDUCE, (obj,));
    Ok((globals(py).make_dict_keys.bind(py), (as_list(obj)?,))
        .into_pyobject(py)?
        .into_any())
}

fn dict_values_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::DICT_VALUES_REDUCE, (obj,));
    Ok((globals(py).make_dict_values.bind(py), (as_list(obj)?,))
        .into_pyobject(py)?
        .into_any())
}

fn dict_items_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::DICT_ITEMS_REDUCE, (obj,));
    Ok((globals(py).make_dict_items.bind(py), (as_dict(obj)?,))
        .into_pyobject(py)?
        .into_any())
}

fn odict_keys_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::ODICT_KEYS_REDUCE, (obj,));
    Ok((globals(py).make_dict_keys.bind(py), (as_list(obj)?, true))
        .into_pyobject(py)?
        .into_any())
}

fn odict_values_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::ODICT_VALUES_REDUCE, (obj,));
    Ok(
        (globals(py).make_dict_values.bind(py), (as_list(obj)?, true))
            .into_pyobject(py)?
            .into_any(),
    )
}

fn odict_items_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::ODICT_ITEMS_REDUCE, (obj,));
    Ok(
        (globals(py).make_dict_items.bind(py), (as_dict(obj)?, true))
            .into_pyobject(py)?
            .into_any(),
    )
}

fn dataclass_field_base_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::DATACLASS_FIELD_BASE_REDUCE, (obj,));
    Ok((
        globals(py).get_dataclass_field_type_sentinel.bind(py),
        (obj.getattr(intern!(py, "name"))?,),
    )
        .into_pyobject(py)?
        .into_any())
}

// ---------------------------------------------------------------------------
// TypeVar
// ---------------------------------------------------------------------------

pub(crate) fn decompose_typevar<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::DECOMPOSE_TYPEVAR, (obj,));
    Ok((
        obj.getattr(intern!(py, "__name__"))?,
        obj.getattr(intern!(py, "__bound__"))?,
        obj.getattr(intern!(py, "__constraints__"))?,
        obj.getattr(intern!(py, "__covariant__"))?,
        obj.getattr(intern!(py, "__contravariant__"))?,
        get_or_create_tracker_id(obj)?,
    )
        .into_pyobject(py)?
        .into_any())
}

fn typevar_reduce<'py>(obj: &Bound<'py, PyAny>) -> R<'py> {
    // TypeVar instances require the module information hence why we
    // are not using the _should_pickle_by_reference directly
    let py = obj.py();
    upstream_if_patched!(py, fallback::TYPEVAR_REDUCE, (obj,));
    let g = globals(py);
    let name = obj.getattr(intern!(py, "__name__"))?;
    let module_and_name = lookup_module_and_qualname(obj, Some(&name))?;
    let by_value = match &module_and_name {
        None => true,
        Some(module_and_name) => {
            is_registered_pickle_by_value(&module_and_name.get_item(0)?)?.is_truthy()?
        }
    };
    match module_and_name {
        Some(module_and_name) if !by_value => Ok((g.getattr.bind(py), module_and_name)
            .into_pyobject(py)?
            .into_any()),
        _ => Ok((g.make_typevar.bind(py), decompose_typevar(obj)?)
            .into_pyobject(py)?
            .into_any()),
    }
}

// ---------------------------------------------------------------------------
// Python wrappers
// ---------------------------------------------------------------------------

/// Python wrapper of a one-argument function; the name of the argument is
/// the one of the upstream function (which can be passed by keyword).
macro_rules! py_reducer {
    ($(#[$doc:meta])* $wrapper:ident, $name:literal, $impl:path) => {
        py_reducer!($(#[$doc])* $wrapper, $name, obj, $impl);
    };
    ($(#[$doc:meta])* $wrapper:ident, $name:literal, $arg:ident, $impl:path) => {
        $(#[$doc])*
        #[pyfunction]
        #[pyo3(name = $name)]
        fn $wrapper<'py>($arg: &Bound<'py, PyAny>) -> R<'py> {
            $impl($arg)
        }
    };
}

py_reducer!(
    /// Retrieve a copy of the dict of a class without the inherited method.
    py_extract_class_dict, "_extract_class_dict", cls, extract_class_dict_any
);
py_reducer!(py_get_bases, "_get_bases", typ, get_bases);
py_reducer!(py_class_getnewargs, "_class_getnewargs", class_getnewargs);
py_reducer!(py_enum_getnewargs, "_enum_getnewargs", enum_getnewargs);
py_reducer!(py_class_getstate, "_class_getstate", class_getstate);
py_reducer!(py_enum_getstate, "_enum_getstate", enum_getstate);
py_reducer!(
    /// Save a class that can't be referenced as a module attribute.
    ///
    /// This method is used to serialize classes that are defined inside
    /// functions, or that otherwise can't be serialized as attribute lookups
    /// from importable modules.
    py_dynamic_class_reduce, "_dynamic_class_reduce", dynamic_class_reduce
);
py_reducer!(
    /// Select the reducer depending on the dynamic nature of the class obj.
    py_class_reduce, "_class_reduce", class_reduce
);
py_reducer!(
    py_get_cell_contents,
    "_get_cell_contents",
    cell,
    get_cell_contents
);
py_reducer!(
    py_function_getstate,
    "_function_getstate",
    func,
    function_getstate
);
py_reducer!(
    /// Cell (containing values of a function's free variables) reducer.
    py_cell_reduce, "_cell_reduce", cell_reduce
);
py_reducer!(
    py_classmethod_reduce,
    "_classmethod_reduce",
    classmethod_reduce
);
py_reducer!(
    /// Save a file.
    py_file_reduce, "_file_reduce", file_reduce
);
py_reducer!(
    py_getset_descriptor_reduce,
    "_getset_descriptor_reduce",
    getset_descriptor_reduce
);
py_reducer!(
    py_mappingproxy_reduce,
    "_mappingproxy_reduce",
    mappingproxy_reduce
);
py_reducer!(
    py_memoryview_reduce,
    "_memoryview_reduce",
    memoryview_reduce
);
py_reducer!(py_module_reduce, "_module_reduce", module_reduce);
py_reducer!(py_method_reduce, "_method_reduce", method_reduce);
py_reducer!(py_logger_reduce, "_logger_reduce", logger_reduce);
py_reducer!(
    py_root_logger_reduce,
    "_root_logger_reduce",
    root_logger_reduce
);
py_reducer!(py_property_reduce, "_property_reduce", property_reduce);
py_reducer!(py_weakset_reduce, "_weakset_reduce", weakset_reduce);
py_reducer!(py_dict_keys_reduce, "_dict_keys_reduce", dict_keys_reduce);
py_reducer!(
    py_dict_values_reduce,
    "_dict_values_reduce",
    dict_values_reduce
);
py_reducer!(
    py_dict_items_reduce,
    "_dict_items_reduce",
    dict_items_reduce
);
py_reducer!(
    py_odict_keys_reduce,
    "_odict_keys_reduce",
    odict_keys_reduce
);
py_reducer!(
    py_odict_values_reduce,
    "_odict_values_reduce",
    odict_values_reduce
);
py_reducer!(
    py_odict_items_reduce,
    "_odict_items_reduce",
    odict_items_reduce
);
py_reducer!(
    py_dataclass_field_base_reduce,
    "_dataclass_field_base_reduce",
    dataclass_field_base_reduce
);
py_reducer!(
    py_decompose_typevar,
    "_decompose_typevar",
    decompose_typevar
);
py_reducer!(py_typevar_reduce, "_typevar_reduce", typevar_reduce);

fn extract_class_dict_any<'py>(cls: &Bound<'py, PyAny>) -> R<'py> {
    Ok(extract_class_dict(cls)?.into_any())
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    add_function(m, wrap_pyfunction!(py_extract_class_dict, m)?)?;
    add_function(m, wrap_pyfunction!(py_get_bases, m)?)?;
    add_function(m, wrap_pyfunction!(py_class_getnewargs, m)?)?;
    add_function(m, wrap_pyfunction!(py_enum_getnewargs, m)?)?;
    add_function(m, wrap_pyfunction!(py_get_cell_contents, m)?)?;
    add_function(m, wrap_pyfunction!(py_function_getstate, m)?)?;
    add_function(m, wrap_pyfunction!(py_class_getstate, m)?)?;
    add_function(m, wrap_pyfunction!(py_enum_getstate, m)?)?;
    add_function(m, wrap_pyfunction!(py_cell_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_classmethod_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_file_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_getset_descriptor_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_mappingproxy_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_memoryview_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_module_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_method_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_logger_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_root_logger_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_property_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_weakset_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_dynamic_class_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_class_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_dict_keys_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_dict_values_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_dict_items_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_odict_keys_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_odict_values_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_odict_items_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_dataclass_field_base_reduce, m)?)?;
    add_function(m, wrap_pyfunction!(py_decompose_typevar, m)?)?;
    add_function(m, wrap_pyfunction!(py_typevar_reduce, m)?)?;
    Ok(())
}
