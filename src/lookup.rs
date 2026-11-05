//! Importability checks: should an object be pickled by reference (as an
//! attribute of an importable module) or by value?
//!
//! Port of `register_pickle_by_value`, `_whichmodule`,
//! `_should_pickle_by_reference`, `_lookup_module_and_qualname` & co.

use pyo3::exceptions::{PyAttributeError, PyException, PyTypeError, PyValueError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyModule, PySet, PyString, PyTuple, PyType};

use crate::fallback::{self, upstream_if_patched};
use crate::state::globals;
use crate::utils::{add_function, getattr_any, getattr_or_none, py_format, str_eq};

type R<'py> = PyResult<Bound<'py, PyAny>>;

// ---------------------------------------------------------------------------
// Dynamic class tracking
// ---------------------------------------------------------------------------

/// Holds the `_DYNAMIC_CLASS_TRACKER_LOCK` (a `threading.Lock`) while alive.
struct TrackerLockGuard<'py> {
    lock: Bound<'py, PyAny>,
}

impl<'py> TrackerLockGuard<'py> {
    fn acquire(py: Python<'py>) -> PyResult<Self> {
        let lock = globals(py).tracker_lock.bind(py).clone();
        lock.call_method0(intern!(py, "acquire"))?;
        Ok(Self { lock })
    }
}

impl Drop for TrackerLockGuard<'_> {
    fn drop(&mut self) {
        let py = self.lock.py();
        if let Err(err) = self.lock.call_method0(intern!(py, "release")) {
            err.write_unraisable(py, Some(&self.lock));
        }
    }
}

pub(crate) fn get_or_create_tracker_id<'py>(
    class_def: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = class_def.py();
    upstream_if_patched!(py, fallback::GET_OR_CREATE_TRACKER_ID, (class_def,));
    let g = globals(py);
    let _guard = TrackerLockGuard::acquire(py)?;
    let by_class = g.tracker_by_class.bind(py);
    let mut class_tracker_id = by_class.call_method1(intern!(py, "get"), (class_def,))?;
    if class_tracker_id.is_none() {
        class_tracker_id = g
            .uuid_module
            .bind(py)
            .getattr(intern!(py, "uuid4"))?
            .call0()?
            .getattr(intern!(py, "hex"))?;
        by_class.set_item(class_def, &class_tracker_id)?;
        g.tracker_by_id
            .bind(py)
            .set_item(&class_tracker_id, class_def)?;
    }
    Ok(class_tracker_id)
}

pub(crate) fn lookup_class_or_track<'py>(
    class_tracker_id: &Bound<'py, PyAny>,
    class_def: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = class_def.py();
    upstream_if_patched!(
        py,
        fallback::LOOKUP_CLASS_OR_TRACK,
        (class_tracker_id, class_def)
    );
    if class_tracker_id.is_none() {
        return Ok(class_def.clone());
    }
    let g = globals(py);
    let _guard = TrackerLockGuard::acquire(py)?;
    let class_def = g
        .tracker_by_id
        .bind(py)
        .call_method1(intern!(py, "setdefault"), (class_tracker_id, class_def))?;
    g.tracker_by_class
        .bind(py)
        .set_item(&class_def, class_tracker_id)?;
    Ok(class_def)
}

#[pyfunction]
#[pyo3(name = "_get_or_create_tracker_id")]
fn py_get_or_create_tracker_id<'py>(class_def: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    get_or_create_tracker_id(class_def)
}

#[pyfunction]
#[pyo3(name = "_lookup_class_or_track")]
fn py_lookup_class_or_track<'py>(
    class_tracker_id: &Bound<'py, PyAny>,
    class_def: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    lookup_class_or_track(class_tracker_id, class_def)
}

// ---------------------------------------------------------------------------
// Pickling by value registry
// ---------------------------------------------------------------------------

fn sys_modules<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
    globals(py).sys.bind(py).getattr(intern!(py, "modules"))
}

/// Register a module to make its functions and classes picklable by value.
///
/// By default, functions and classes that are attributes of an importable
/// module are to be pickled by reference, that is relying on re-importing
/// the attribute from the module at load time.
///
/// If `register_pickle_by_value(module)` is called, all its functions and
/// classes are subsequently to be pickled by value, meaning that they can
/// be loaded in Python processes where the module is not importable.
///
/// This is especially useful when developing a module in a distributed
/// execution environment: restarting the client Python process with the new
/// source code is enough: there is no need to re-install the new version
/// of the module on all the worker nodes nor to restart the workers.
///
/// Note: this feature is considered experimental. See the README.md file for
/// more details and limitations.
#[pyfunction]
fn register_pickle_by_value<'py>(module: &Bound<'py, PyAny>) -> R<'py> {
    let py = module.py();
    upstream_if_patched!(py, fallback::REGISTER_PICKLE_BY_VALUE, (module,));
    let g = globals(py);
    if !module.is_instance(g.module_type.bind(py))? {
        return Err(PyValueError::new_err(format!(
            "Input should be a module object, got {} instead",
            module.str()?
        )));
    }
    // In the future, cloudpickle may need a way to access any module registered
    // for pickling by value in order to introspect relative imports inside
    // functions pickled by value. (see
    // https://github.com/cloudpipe/cloudpickle/pull/417#issuecomment-873684633).
    // This access can be ensured by checking that module is present in
    // sys.modules at registering time and assuming that it will still be in
    // there when accessed during pickling. Another alternative would be to
    // store a weakref to the module. Even though cloudpickle does not implement
    // this introspection yet, in order to avoid a possible breaking change
    // later, we still enforce the presence of module inside sys.modules.
    let name = module.getattr(intern!(py, "__name__"))?;
    if !sys_modules(py)?.contains(&name)? {
        return Err(PyValueError::new_err(format!(
            "{} was not imported correctly, have you used an `import` statement to access it?",
            py_format(module)?
        )));
    }
    g.pickle_by_value_modules.bind(py).add(name)?;
    Ok(py.None().into_bound(py))
}

/// Unregister that the input module should be pickled by value.
#[pyfunction]
fn unregister_pickle_by_value<'py>(module: &Bound<'py, PyAny>) -> R<'py> {
    let py = module.py();
    upstream_if_patched!(py, fallback::UNREGISTER_PICKLE_BY_VALUE, (module,));
    let g = globals(py);
    if !module.is_instance(g.module_type.bind(py))? {
        return Err(PyValueError::new_err(format!(
            "Input should be a module object, got {} instead",
            module.str()?
        )));
    }
    let registry = g.pickle_by_value_modules.bind(py);
    let name = module.getattr(intern!(py, "__name__"))?;
    if !registry.contains(&name)? {
        return Err(PyValueError::new_err(format!(
            "{} is not registered for pickle by value",
            py_format(module)?
        )));
    }
    registry.call_method1(intern!(py, "remove"), (name,))?;
    Ok(py.None().into_bound(py))
}

#[pyfunction]
fn list_registry_pickle_by_value<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
    upstream_if_patched!(py, fallback::LIST_REGISTRY_PICKLE_BY_VALUE, ());
    globals(py)
        .pickle_by_value_modules
        .bind(py)
        .call_method0(intern!(py, "copy"))
}

/// `_is_registered_pickle_by_value(module)`: its result (a bool) as returned
/// by the upstream function, whose callers test its truth value.
pub(crate) fn is_registered_pickle_by_value<'py>(module: &Bound<'py, PyAny>) -> R<'py> {
    let py = module.py();
    upstream_if_patched!(py, fallback::IS_REGISTERED_PICKLE_BY_VALUE, (module,));
    Ok(PyBool::new(py, is_registered(module)?)
        .to_owned()
        .into_any())
}

pub(crate) fn is_registered(module: &Bound<'_, PyAny>) -> PyResult<bool> {
    let py = module.py();
    let registry = globals(py).pickle_by_value_modules.bind(py);
    let module_name = module.getattr(intern!(py, "__name__"))?;
    if registry.contains(&module_name)? {
        return Ok(true);
    }
    if let Ok(name) = module_name.cast::<PyString>() {
        if let Ok(name) = name.to_str() {
            return is_registered_name(registry, name);
        }
    }
    // Generic fallback mirroring the Python code exactly (`__name__` is not
    // a str, or cannot be encoded to UTF-8).
    let mut module_name = module_name;
    loop {
        let parent_name = module_name
            .call_method1(intern!(py, "rsplit"), (".", 1))?
            .get_item(0)?;
        if parent_name.eq(&module_name)? {
            return Ok(false);
        }
        if registry.contains(&parent_name)? {
            return Ok(true);
        }
        module_name = parent_name;
    }
}

/// Is `name` or any of its parent packages registered (the full name itself
/// is checked by the caller)?
fn is_registered_name(registry: &Bound<'_, PySet>, name: &str) -> PyResult<bool> {
    if registry.is_empty() {
        return Ok(false);
    }
    let py = registry.py();
    let mut module_name = name;
    // `module_name.rsplit(".", 1)[0]` until it stops changing.
    while let Some(idx) = module_name.rfind('.') {
        let parent_name = &module_name[..idx];
        if registry.contains(PyString::new(py, parent_name))? {
            return Ok(true);
        }
        module_name = parent_name;
    }
    Ok(false)
}

#[pyfunction]
#[pyo3(name = "_is_registered_pickle_by_value")]
fn py_is_registered_pickle_by_value<'py>(module: &Bound<'py, PyAny>) -> R<'py> {
    is_registered_pickle_by_value(module)
}

// ---------------------------------------------------------------------------
// Attribute lookups
// ---------------------------------------------------------------------------

/// The components of a dotted name, i.e. `name.split(".")`.
fn split_dotted<'py>(name: &Bound<'py, PyAny>) -> PyResult<Vec<Bound<'py, PyAny>>> {
    let py = name.py();
    if let Ok(s) = name.cast_exact::<PyString>() {
        if let Ok(s) = s.to_str() {
            return Ok(s
                .split('.')
                .map(|part| PyString::new(py, part).into_any())
                .collect());
        }
    }
    name.call_method1(intern!(py, "split"), (".",))?
        .try_iter()?
        .collect()
}

/// Outcome of a lookup that failed with an `AttributeError` raised (with a
/// formatted message) by `pickle._getattribute` on Python < 3.14.
struct AttributeMiss<'py> {
    /// The object on which the failed attribute lookup happened.
    on: Bound<'py, PyAny>,
    /// Whether the failure is due to a `<locals>` component.
    local: bool,
}

/// Port of `_getattribute(obj, name)` (dotted attribute lookup, based on the
/// private `pickle._getattribute` whose behavior depends on the Python
/// version).
///
/// The `AttributeError`s raised by `pickle._getattribute` on Python < 3.14
/// are returned as `Ok(Err(miss))` so that internal callers, which ignore
/// them, do not pay for the formatting of their messages (which includes the
/// repr of the object).
fn getattribute_parts<'py>(
    obj: &Bound<'py, PyAny>,
    parts: &[Bound<'py, PyAny>],
) -> PyResult<Result<Bound<'py, PyAny>, AttributeMiss<'py>>> {
    let py = obj.py();
    let mut current = obj.clone();
    if globals(py).at_least(3, 14) {
        for part in parts {
            current = getattr_any(&current, part)?;
        }
        return Ok(Ok(current));
    }
    for part in parts {
        if str_eq(part, "<locals>")? {
            return Ok(Err(AttributeMiss {
                on: current,
                local: true,
            }));
        }
        match getattr_any(&current, part) {
            Ok(value) => current = value,
            Err(err) if err.is_instance_of::<PyAttributeError>(py) => {
                return Ok(Err(AttributeMiss {
                    on: current,
                    local: false,
                }));
            }
            Err(err) => return Err(err),
        }
    }
    Ok(Ok(current))
}

#[pyfunction]
#[pyo3(name = "_getattribute")]
fn py_getattribute<'py>(
    obj: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = obj.py();
    upstream_if_patched!(py, fallback::GETATTRIBUTE, (obj, name));
    let parts = split_dotted(name)?;
    match getattribute_parts(obj, &parts)? {
        Ok(value) => Ok(value),
        Err(miss) => {
            // The message of `pickle._getattribute` refers to the object on
            // which the lookup failed until Python 3.11, and to the object on
            // which the dotted lookup started in Python 3.12 and 3.13.
            let on = if globals(py).at_least(3, 12) {
                obj
            } else {
                &miss.on
            };
            Err(PyAttributeError::new_err(format!(
                "Can't get {}attribute {} on {}",
                if miss.local { "local " } else { "" },
                name.repr()?,
                on.repr()?
            )))
        }
    }
}

/// Find the module an object belongs to.
///
/// This function differs from ``pickle.whichmodule`` in two ways:
/// - it does not mangle the cases where obj's module is __main__ and obj was
///   not found in any module.
/// - Errors arising during module introspection are ignored, as those errors
///   are considered unwanted side effects.
pub(crate) fn whichmodule<'py>(
    obj: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let py = obj.py();
    if !fallback::WHICHMODULE.pristine(py)? {
        let module_name = fallback::WHICHMODULE.call(py, (obj, name), None)?;
        return Ok(Some(module_name).filter(|m| !m.is_none()));
    }
    if let Some(module_name) = getattr_or_none(obj, intern!(py, "__module__"))? {
        return Ok(Some(module_name));
    }
    let g = globals(py);
    // `name.split(".")` happens inside the `try` block of the Python
    // implementation: if it fails, no module can match.
    let parts = match split_dotted(name) {
        Ok(parts) => Some(parts),
        Err(err) if err.is_instance_of::<PyException>(py) => None,
        Err(err) => return Err(err),
    };
    // Protect the iteration by using a copy of sys.modules against dynamic
    // modules that trigger imports of other modules upon calls to getattr or
    // other threads importing at the same time.
    let modules = sys_modules(py)?.call_method0(intern!(py, "copy"))?;
    let module_type = g.module_type.bind(py);
    let check = |module_name: Bound<'py, PyAny>,
                 module: Bound<'py, PyAny>|
     -> PyResult<Option<Bound<'py, PyAny>>> {
        // Some modules such as coverage can inject non-module objects inside
        // sys.modules
        if str_eq(&module_name, "__main__")?
            || str_eq(&module_name, "__mp_main__")?
            || module.is_none()
            || !module.is_instance(module_type)?
        {
            return Ok(None);
        }
        let Some(parts) = parts.as_deref() else {
            return Ok(None);
        };
        match getattribute_parts(&module, parts) {
            Ok(Ok(value)) if value.is(obj) => Ok(Some(module_name)),
            Ok(_) => Ok(None),
            Err(err) if err.is_instance_of::<PyException>(py) => Ok(None),
            Err(err) => Err(err),
        }
    };
    if let Ok(modules) = modules.cast::<PyDict>() {
        // `modules` is a private copy: it cannot be mutated while iterating.
        for (module_name, module) in modules.iter() {
            if let Some(found) = check(module_name, module)? {
                return Ok(Some(found));
            }
        }
    } else {
        for item in modules.call_method0(intern!(py, "items"))?.try_iter()? {
            let (module_name, module) = crate::utils::unpack2(&item?)?;
            if let Some(found) = check(module_name, module)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

#[pyfunction]
#[pyo3(name = "_whichmodule")]
fn py_whichmodule<'py>(
    obj: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    whichmodule(obj, name)
}

/// `sys.modules.get(module_name, None)`
fn sys_modules_get<'py>(
    py: Python<'py>,
    module_name: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let modules = sys_modules(py)?;
    let module = match modules.cast::<PyDict>() {
        Ok(modules) => modules.get_item(module_name)?,
        Err(_) => Some(modules.call_method1(intern!(py, "get"), (module_name, py.None()))?),
    };
    Ok(module.filter(|m| !m.is_none()))
}

/// `_lookup_module_and_qualname(obj, name)`: None or the `(module, name)`
/// tuple (as returned by the upstream function, whose callers unpack it or
/// use it as is).
pub(crate) fn lookup_module_and_qualname<'py>(
    obj: &Bound<'py, PyAny>,
    name: Option<&Bound<'py, PyAny>>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let py = obj.py();
    if !fallback::LOOKUP_MODULE_AND_QUALNAME.pristine(py)? {
        let none = py.None().into_bound(py);
        let module_and_name =
            fallback::LOOKUP_MODULE_AND_QUALNAME.call(py, (obj, name.unwrap_or(&none)), None)?;
        return Ok(Some(module_and_name).filter(|m| !m.is_none()));
    }
    let mut name = name.filter(|n| !n.is_none()).cloned();
    if name.is_none() {
        name = getattr_or_none(obj, intern!(py, "__qualname__"))?;
    }
    if name.is_none() {
        // This used to be needed for Python 2.7 support but is probably not
        // needed anymore. However we keep the __name__ introspection in case
        // users of cloudpickle rely on this old behavior for unknown reasons.
        name = getattr_or_none(obj, intern!(py, "__name__"))?;
    }
    let name = name.unwrap_or_else(|| py.None().into_bound(py));

    let Some(module_name) = whichmodule(obj, &name)? else {
        // In this case, obj.__module__ is None AND obj was not found in any
        // imported module. obj is thus treated as dynamic.
        return Ok(None);
    };
    if str_eq(&module_name, "__main__")? {
        return Ok(None);
    }
    // Note: if module_name is in sys.modules, the corresponding module is
    // assumed importable at unpickling time. See #357
    let Some(module) = sys_modules_get(py, &module_name)? else {
        // The main reason why obj's module would not be imported is that this
        // module has been dynamically created, using for example
        // types.ModuleType. The other possibility is that module was removed
        // from sys.modules after obj was created/imported. But this case is not
        // supported, as the standard pickle does not support it either.
        return Ok(None);
    };
    // `name.split(".")` is evaluated inside the `try` block of the Python
    // implementation, which catches `AttributeError` (e.g. `name` is None).
    let parts = match split_dotted(&name) {
        Ok(parts) => parts,
        Err(err) if err.is_instance_of::<PyAttributeError>(py) => return Ok(None),
        Err(err) => return Err(err),
    };
    match getattribute_parts(&module, &parts) {
        Ok(Ok(obj2)) if obj2.is(obj) => Ok(Some(PyTuple::new(py, [module, name])?.into_any())),
        // obj was not found inside the module it points to
        Ok(_) => Ok(None),
        Err(err) if err.is_instance_of::<PyAttributeError>(py) => Ok(None),
        Err(err) => Err(err),
    }
}

#[pyfunction]
#[pyo3(name = "_lookup_module_and_qualname", signature = (obj, name=None))]
fn py_lookup_module_and_qualname<'py>(
    obj: &Bound<'py, PyAny>,
    name: Option<&Bound<'py, PyAny>>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    lookup_module_and_qualname(obj, name)
}

/// Test whether an function or a class should be pickled by reference
///
/// Pickling by reference means by that the object (typically a function or a
/// class) is an attribute of a module that is assumed to be importable in the
/// target Python environment. Loading will therefore rely on importing the
/// module and then calling `getattr` on it to access the function or class.
///
/// Pickling by reference is the only option to pickle functions and classes
/// in the standard library. In cloudpickle the alternative option is to
/// pickle by value (for instance for interactively or locally defined
/// functions and classes or for attributes of modules that have been
/// explicitly registered to be pickled by value.
///
/// Returns the result of the upstream function (a bool), whose callers test
/// its truth value.
pub(crate) fn should_pickle_by_reference<'py>(
    obj: &Bound<'py, PyAny>,
    name: Option<&Bound<'py, PyAny>>,
) -> R<'py> {
    let py = obj.py();
    if !fallback::SHOULD_PICKLE_BY_REFERENCE.pristine(py)? {
        let none = py.None().into_bound(py);
        return fallback::SHOULD_PICKLE_BY_REFERENCE.call(py, (obj, name.unwrap_or(&none)), None);
    }
    let g = globals(py);
    let bool_ = |value: bool| PyBool::new(py, value).to_owned().into_any();
    if obj.is_instance(g.function_type.bind(py))? || obj.is_instance_of::<PyType>() {
        let Some(module_and_name) = lookup_module_and_qualname(obj, name)? else {
            return Ok(bool_(false));
        };
        let [module, _name]: [Bound<'py, PyAny>; 2] = fallback::unpack(&module_and_name, 2)?
            .try_into()
            .expect("two values");
        return Ok(bool_(!is_registered_pickle_by_value(&module)?.is_truthy()?));
    }
    if obj.is_instance(g.module_type.bind(py))? {
        // We assume that sys.modules is primarily used as a cache mechanism for
        // the Python import machinery. Checking if a module has been added in
        // is sys.modules therefore a cheap and simple heuristic to tell us
        // whether we can assume that a given module could be imported by name
        // in another Python process.
        if is_registered_pickle_by_value(obj)?.is_truthy()? {
            return Ok(bool_(false));
        }
        let name = obj.getattr(intern!(py, "__name__"))?;
        return Ok(bool_(sys_modules(py)?.contains(name)?));
    }
    Err(PyTypeError::new_err(format!(
        "cannot check importability of {} instances",
        obj.get_type().name()?
    )))
}

#[pyfunction]
#[pyo3(name = "_should_pickle_by_reference", signature = (obj, name=None))]
fn py_should_pickle_by_reference<'py>(
    obj: &Bound<'py, PyAny>,
    name: Option<&Bound<'py, PyAny>>,
) -> R<'py> {
    should_pickle_by_reference(obj, name)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    add_function(m, wrap_pyfunction!(py_get_or_create_tracker_id, m)?)?;
    add_function(m, wrap_pyfunction!(py_lookup_class_or_track, m)?)?;
    add_function(m, wrap_pyfunction!(register_pickle_by_value, m)?)?;
    add_function(m, wrap_pyfunction!(unregister_pickle_by_value, m)?)?;
    add_function(m, wrap_pyfunction!(list_registry_pickle_by_value, m)?)?;
    add_function(m, wrap_pyfunction!(py_is_registered_pickle_by_value, m)?)?;
    add_function(m, wrap_pyfunction!(py_getattribute, m)?)?;
    add_function(m, wrap_pyfunction!(py_whichmodule, m)?)?;
    add_function(m, wrap_pyfunction!(py_should_pickle_by_reference, m)?)?;
    add_function(m, wrap_pyfunction!(py_lookup_module_and_qualname, m)?)?;
    Ok(())
}
