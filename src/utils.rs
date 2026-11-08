//! Small helpers shared by the rest of the crate.
//!
//! Most of them exist to reproduce *exactly* the semantics of the Python
//! expressions used by upstream `cloudpickle.py` (for instance `"".join(s)`,
//! `d.pop(k, None)` or `a, b = obj`), so that the Rust implementation behaves
//! like the original one, including in corner cases.

use std::ffi::CStr;
use std::os::raw::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};

use pyo3::exceptions::{PyKeyError, PyTypeError};
use pyo3::ffi;
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict, PyModule, PyString, PyTuple, PyType};

/// Equivalent of Python's `"".join(s)`.
///
/// cloudpickle relies on `"".join(name)` to obtain a *new*, non-interned copy
/// of a string: the Pickler memoizes objects by identity, so sharing (or not)
/// string objects between different parts of a payload changes the produced
/// bytes. Using fresh copies makes the payload independent of the interning
/// state of the process that produced it.
pub(crate) fn fresh_str<'py>(s: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = s.py();
    if let Ok(pystr) = s.cast_exact::<PyString>() {
        if let Ok(rs) = pystr.to_str() {
            // For strings of length >= 2 this creates a new object, while
            // empty and single latin-1 character strings are singletons in
            // CPython: exactly like `"".join(s)`.
            return Ok(PyString::new(py, rs).into_any());
        }
    }
    // Generic (and exact) fallback: str subclasses, strings that cannot be
    // encoded to UTF-8 (lone surrogates), non-string iterables...
    let empty = PyString::new(py, "");
    unsafe { Bound::from_owned_ptr_or_err(py, ffi::PyUnicode_Join(empty.as_ptr(), s.as_ptr())) }
}

/// `obj == s` where `s` is a Rust string, with a fast path for exact `str`.
pub(crate) fn str_eq(obj: &Bound<'_, PyAny>, s: &str) -> PyResult<bool> {
    if let Ok(pystr) = obj.cast_exact::<PyString>() {
        // A string that cannot be encoded to UTF-8 contains lone surrogates
        // and is therefore different from any Rust string.
        return Ok(matches!(pystr.to_str(), Ok(v) if v == s));
    }
    obj.eq(s)
}

/// `getattr(obj, name, None)` returning `None` both when the attribute is
/// missing and when its value is `None`.
pub(crate) fn getattr_or_none<'py>(
    obj: &Bound<'py, PyAny>,
    name: &Bound<'py, PyString>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    Ok(obj.getattr_opt(name)?.filter(|v| !v.is_none()))
}

/// `getattr(obj, name)` for an arbitrary `name` object (exact builtin
/// semantics, including the `TypeError` raised for non-string names).
pub(crate) fn getattr_any<'py>(
    obj: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    unsafe {
        Bound::from_owned_ptr_or_err(obj.py(), ffi::PyObject_GetAttr(obj.as_ptr(), name.as_ptr()))
    }
}

/// `setattr(obj, name, value)` for an arbitrary `name` object.
pub(crate) fn setattr_any(
    obj: &Bound<'_, PyAny>,
    name: &Bound<'_, PyAny>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let r = unsafe { ffi::PyObject_SetAttr(obj.as_ptr(), name.as_ptr(), value.as_ptr()) };
    if r < 0 {
        Err(PyErr::fetch(obj.py()))
    } else {
        Ok(())
    }
}

/// `id(obj)`: the address of the object on CPython (computed without a call),
/// the builtin on other implementations.
pub(crate) fn py_id<'py>(obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = obj.py();
    let g = crate::state::globals(py);
    if g.pypy {
        g.id.bind(py).call1((obj,))
    } else {
        Ok((obj.as_ptr() as usize).into_pyobject(py)?.into_any())
    }
}

/// `issubclass(derived, cls)` (works for any object, like the builtin).
pub(crate) fn is_subclass(derived: &Bound<'_, PyAny>, cls: &Bound<'_, PyAny>) -> PyResult<bool> {
    let r = unsafe { ffi::PyObject_IsSubclass(derived.as_ptr(), cls.as_ptr()) };
    if r < 0 {
        Err(PyErr::fetch(derived.py()))
    } else {
        Ok(r == 1)
    }
}

/// `d.pop(key, None)` on an exact dict.
pub(crate) fn dict_pop<'py>(
    d: &Bound<'py, PyDict>,
    key: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    match d.get_item(key)? {
        Some(value) => {
            d.del_item(key)?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

/// `d.pop(key)` on an exact dict: raises `KeyError` if `key` is missing.
pub(crate) fn dict_pop_required<'py>(
    d: &Bound<'py, PyDict>,
    key: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    dict_pop(d, key)?.ok_or_else(|| PyKeyError::new_err((key.clone().unbind(),)))
}

/// `a, b = obj` (iterable unpacking of exactly two values, with the error
/// messages of the running Python version).
pub(crate) fn unpack2<'py>(
    obj: &Bound<'py, PyAny>,
) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
    let mut values = crate::fallback::unpack(obj, 2)?.into_iter();
    match (values.next(), values.next()) {
        (Some(a), Some(b)) => Ok((a, b)),
        _ => unreachable!("unpacking returns two values"),
    }
}

/// `format(obj, "")`: how an f-string interpolates `obj`.
pub(crate) fn py_format<'py>(obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = obj.py();
    let empty = PyString::new(py, "");
    unsafe { Bound::from_owned_ptr_or_err(py, ffi::PyObject_Format(obj.as_ptr(), empty.as_ptr())) }
}

/// Iterate over `mapping.items()` using the Python protocol (safe even if
/// arbitrary Python code runs during the iteration).
pub(crate) fn for_each_item<'py>(
    mapping: &Bound<'py, PyAny>,
    mut f: impl FnMut(Bound<'py, PyAny>, Bound<'py, PyAny>) -> PyResult<()>,
) -> PyResult<()> {
    let py = mapping.py();
    for item in mapping
        .call_method0(pyo3::intern!(py, "items"))?
        .try_iter()?
    {
        let (k, v) = unpack2(&item?)?;
        f(k, v)?;
    }
    Ok(())
}

/// `raise new from cause`: sets both `__cause__` and `__context__`.
pub(crate) fn raise_from(py: Python<'_>, new: PyErr, cause: PyErr) -> PyErr {
    let context = cause.value(py).clone();
    // SAFETY: PyException_SetContext steals the reference to `context`.
    unsafe { ffi::PyException_SetContext(new.value(py).as_ptr(), context.into_ptr()) };
    new.set_cause(py, Some(cause));
    new
}

/// Set a module attribute without registering it in `__all__` (unlike
/// `PyModule::add`): the upstream `cloudpickle.cloudpickle` module does not
/// define `__all__`, which matters for `from .cloudpickle import *`.
pub(crate) fn set_attr<'py>(
    m: &Bound<'py, PyModule>,
    name: &str,
    value: impl IntoPyObject<'py>,
) -> PyResult<()> {
    m.setattr(name, value)
}

/// Whether the compiler removes the docstrings (`python -OO`). The module and
/// the classes created by cloudpickle-rust then have no docstring either,
/// like upstream's module, whose docstrings the compiler removes.
pub(crate) fn docstrings_removed(py: Python<'_>) -> PyResult<bool> {
    let optimize: i64 = py
        .import("sys")?
        .getattr("flags")?
        .getattr("optimize")?
        .extract()?;
    Ok(optimize >= 2)
}

/// Register a `#[pyfunction]` in the module under its Python name.
pub(crate) fn add_function(m: &Bound<'_, PyModule>, f: Bound<'_, PyCFunction>) -> PyResult<()> {
    let name = f.getattr(pyo3::intern!(m.py(), "__name__"))?;
    m.setattr(name.cast_into::<PyString>()?, f)
}

// ---------------------------------------------------------------------------
// Raw method descriptors
// ---------------------------------------------------------------------------
//
// The cloudpickle `Pickler` is a Python-level subclass of the C
// `_pickle.Pickler` type, which PyO3's `#[pyclass(extends = ...)]` cannot
// express. We therefore create the class with `type(...)` and attach real
// method descriptors (the same kind of objects as the methods of builtin
// types) to it with `PyDescr_NewMethod`. Unlike a plain builtin function
// stored in a class `__dict__`, such descriptors bind to instances, and the
// C pickler calls them through the fastest calling conventions.

/// Run `body` at the FFI boundary: converts Rust panics and `PyErr`s into
/// Python exceptions.
///
/// # Safety
/// Must only be called by CPython (the thread is attached to the interpreter).
pub(crate) unsafe fn trampoline<F>(body: F) -> *mut ffi::PyObject
where
    F: for<'py> FnOnce(Python<'py>) -> PyResult<Bound<'py, PyAny>>,
{
    let py = unsafe { Python::assume_attached() };
    match catch_unwind(AssertUnwindSafe(move || body(py))) {
        Ok(Ok(obj)) => obj.into_ptr(),
        Ok(Err(err)) => {
            err.restore(py);
            std::ptr::null_mut()
        }
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else {
                "panic from Rust code".to_string()
            };
            PanicException::new_err((msg,)).restore(py);
            std::ptr::null_mut()
        }
    }
}

/// Signature of a `METH_FASTCALL | METH_KEYWORDS` method implementation.
pub(crate) type MethFastcall = unsafe extern "C" fn(
    *mut ffi::PyObject,
    *const *mut ffi::PyObject,
    ffi::Py_ssize_t,
    *mut ffi::PyObject,
) -> *mut ffi::PyObject;

#[cfg(not(PyPy))]
use ffi::PyDescr_NewMethod;

#[cfg(PyPy)]
extern "C" {
    #[link_name = "PyPyDescr_NewMethod"]
    fn PyDescr_NewMethod(
        tp: *mut ffi::PyTypeObject,
        def: *mut ffi::PyMethodDef,
    ) -> *mut ffi::PyObject;
}

/// Parameters of a method implemented in Rust, used to parse the arguments
/// of `METH_FASTCALL | METH_KEYWORDS` calls with the semantics (and error
/// messages) of a Python function `def name(self, *names)` whose first
/// `required` parameters have no default value.
pub(crate) struct Params {
    pub qualname: &'static str,
    pub names: &'static [&'static str],
    pub required: usize,
}

impl Params {
    /// Parse the arguments of a vectorcall into one slot per parameter.
    ///
    /// # Safety
    /// `args`, `nargs` and `kwnames` must come from CPython's vectorcall
    /// protocol.
    pub(crate) unsafe fn parse<'py>(
        &self,
        py: Python<'py>,
        args: *const *mut ffi::PyObject,
        nargs: ffi::Py_ssize_t,
        kwnames: *mut ffi::PyObject,
    ) -> PyResult<Vec<Option<Bound<'py, PyAny>>>> {
        let nargs = nargs as usize;
        let names = self.names;
        if nargs > names.len() {
            let accepted = if self.required == names.len() {
                format!("{}", names.len() + 1)
            } else {
                format!("from {} to {}", self.required + 1, names.len() + 1)
            };
            return Err(PyTypeError::new_err(format!(
                "{}() takes {} positional arguments but {} were given",
                self.qualname,
                accepted,
                nargs + 1
            )));
        }
        let mut slots: Vec<Option<Bound<'py, PyAny>>> = vec![None; names.len()];
        for (i, slot) in slots.iter_mut().enumerate().take(nargs) {
            *slot = Some(unsafe { Bound::from_borrowed_ptr(py, *args.add(i)) });
        }
        if !kwnames.is_null() {
            let kwnames = unsafe { Bound::from_borrowed_ptr(py, kwnames) };
            let kwnames = kwnames.cast::<PyTuple>()?;
            for (j, keyword) in kwnames.iter().enumerate() {
                let value = unsafe { Bound::from_borrowed_ptr(py, *args.add(nargs + j)) };
                let keyword = keyword.cast_into::<PyString>()?;
                let position = names
                    .iter()
                    .position(|n| matches!(keyword.to_str(), Ok(k) if k == *n));
                match position {
                    Some(i) if slots[i].is_some() => {
                        return Err(PyTypeError::new_err(format!(
                            "{}() got multiple values for argument '{}'",
                            self.qualname, keyword
                        )));
                    }
                    Some(i) => slots[i] = Some(value),
                    None => {
                        return Err(PyTypeError::new_err(format!(
                            "{}() got an unexpected keyword argument '{}'",
                            self.qualname, keyword
                        )));
                    }
                }
            }
        }
        let missing: Vec<&str> = (0..self.required)
            .filter(|&i| slots[i].is_none())
            .map(|i| names[i])
            .collect();
        if !missing.is_empty() {
            let quoted: Vec<String> = missing.iter().map(|n| format!("'{n}'")).collect();
            let list = match quoted.len() {
                1 => quoted[0].clone(),
                2 => format!("{} and {}", quoted[0], quoted[1]),
                n => format!("{}, and {}", quoted[..n - 1].join(", "), quoted[n - 1]),
            };
            return Err(PyTypeError::new_err(format!(
                "{}() missing {} required positional argument{}: {}",
                self.qualname,
                missing.len(),
                if missing.len() == 1 { "" } else { "s" },
                list
            )));
        }
        Ok(slots)
    }
}

/// Attach a method descriptor named `name` to `cls`.
///
/// The `PyMethodDef` is leaked on purpose: it must outlive the descriptor,
/// which lives as long as the class (i.e. until the end of the process).
/// Returns the descriptor, which is also stored in the class namespace.
pub(crate) fn add_method<'py>(
    cls: &Bound<'py, PyType>,
    name: &'static CStr,
    meth: MethFastcall,
    doc: &'static CStr,
) -> PyResult<Bound<'py, PyAny>> {
    let ml_flags: c_int = ffi::METH_FASTCALL | ffi::METH_KEYWORDS;
    let def = Box::into_raw(Box::new(ffi::PyMethodDef {
        ml_name: name.as_ptr(),
        ml_meth: ffi::PyMethodDefPointer {
            PyCFunctionFastWithKeywords: meth,
        },
        ml_flags,
        ml_doc: doc.as_ptr(),
    }));
    let py = cls.py();
    let descr = unsafe {
        Bound::from_owned_ptr_or_err(
            py,
            PyDescr_NewMethod(cls.as_ptr().cast::<ffi::PyTypeObject>(), def),
        )?
    };
    // `type.__setattr__` also updates the type slots (`tp_init`,
    // `mp_subscript`, ...) when a special method is (re)defined.
    cls.setattr(name.to_str().expect("method names are ASCII"), &descr)?;
    Ok(descr)
}

/// Define the `METH_FASTCALL | METH_KEYWORDS` C function `$cname` of a method
/// with parameters `$params`, implemented by `$impl(&slf, &args)` where `args`
/// holds one `Option<Bound<PyAny>>` per parameter.
///
/// Calls with exactly the required positional arguments (by far the most
/// common case, e.g. when the C pickler calls `reducer_override(obj)`) skip
/// the generic argument parsing.
macro_rules! fastcall_method {
    ($cname:ident, $params:expr, $impl:path) => {
        unsafe extern "C" fn $cname(
            slf: *mut pyo3::ffi::PyObject,
            args: *const *mut pyo3::ffi::PyObject,
            nargs: pyo3::ffi::Py_ssize_t,
            kwnames: *mut pyo3::ffi::PyObject,
        ) -> *mut pyo3::ffi::PyObject {
            unsafe {
                $crate::utils::trampoline(|py| {
                    let params: &$crate::utils::Params = &$params;
                    let slf = pyo3::Bound::from_borrowed_ptr(py, slf);
                    let parsed = if kwnames.is_null() && nargs as usize == params.names.len() {
                        (0..params.names.len())
                            .map(|i| Some(pyo3::Bound::from_borrowed_ptr(py, *args.add(i))))
                            .collect::<Vec<_>>()
                    } else {
                        params.parse(py, args, nargs, kwnames)?
                    };
                    $impl(&slf, &parsed)
                })
            }
        }
    };
}
pub(crate) use fastcall_method;
