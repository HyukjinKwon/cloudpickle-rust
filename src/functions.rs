//! The functions of `cloudpickle_rust.cloudpickle` are Python functions, like
//! upstream's: each one has the signature (parameters and default values) and
//! the docstring of the upstream function, the namespace of the module as its
//! `__globals__`, and calls its Rust implementation, which it keeps in its
//! closure.
//!
//! They are thus instances of `types.FunctionType`, which matters to the
//! pickler (`Pickler.reducer_override` and the upstream code run by
//! `fallback` check for it), to `inspect`, and to warnings (attributed to the
//! frame of the function, i.e. to the module, like upstream's).

use std::collections::HashMap;

use pyo3::exceptions::PyRuntimeError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule, PyString, PyTuple, PyType};

use crate::upstream_docs;

const FILENAME: &str = "<cloudpickle_rust>";

struct Param {
    name: String,
    /// The Python expression of the default value.
    default: Option<String>,
}

/// The parameters of a signature such as `(obj, name=None)`.
fn parse_params(signature: &str) -> PyResult<Vec<Param>> {
    let inner = signature
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| PyRuntimeError::new_err(format!("unexpected signature {signature}")))?;
    let mut params = Vec::new();
    for part in inner.split(',').map(str::trim) {
        if part.is_empty() || part.starts_with('$') || part == "/" {
            continue;
        }
        params.push(match part.split_once('=') {
            Some((name, default)) => Param {
                name: name.trim().into(),
                default: Some(default.trim().into()),
            },
            None => Param {
                name: part.into(),
                default: None,
            },
        });
    }
    Ok(params)
}

/// A Python string literal of value `s`.
fn python_literal(s: &str) -> String {
    let mut literal = String::with_capacity(s.len() + 2);
    literal.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                literal.push_str(&format!("\\U{:08x}", c as u32))
            }
            c => literal.push(c),
        }
    }
    literal.push('"');
    literal
}

/// The docstring of a function whose docstring literal has the value `raw`,
/// as the running Python compiler produces it (Python 3.13 and later strip
/// the indentation of docstrings): `None` when the compiler removes the
/// docstrings (`python -OO`), as for upstream's functions.
pub(crate) fn compiled_doc(py: Python<'_>, raw: &str) -> PyResult<Option<String>> {
    let source = format!("def f():\n    {}\n", python_literal(raw));
    let namespace = PyDict::new(py);
    py.run(
        &std::ffi::CString::new(source).expect("no NUL byte in docstrings"),
        Some(&namespace),
        None,
    )?;
    namespace
        .get_item("f")?
        .ok_or_else(|| PyRuntimeError::new_err("docstring stub"))?
        .getattr("__doc__")?
        .extract()
}

/// A Python function to create: `qualname(<params>)` calling `native`.
struct Wrapper<'py> {
    name: String,
    qualname: String,
    params: Vec<Param>,
    native: Bound<'py, PyAny>,
    doc: Option<&'static str>,
}

/// Create the Python functions, with the namespace of the module as their
/// globals.
fn build<'py>(
    module_dict: &Bound<'py, PyDict>,
    wrappers: Vec<Wrapper<'py>>,
) -> PyResult<Vec<(String, Bound<'py, PyAny>)>> {
    let py = module_dict.py();
    let builtins = py.import("builtins")?;
    let function_type = py.import("types")?.getattr("FunctionType")?;

    // def <name>(__native):
    //     def <name>(<parameters>):
    //         "<docstring>"
    //         return __native(<parameters>)
    //     return <name>
    //
    // (the docstring is processed by the compiler like upstream's)
    let mut source = String::new();
    for wrapper in &wrappers {
        let name = &wrapper.name;
        let names = wrapper
            .params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let doc = match wrapper.doc {
            Some(doc) => format!("        {}\n", python_literal(doc)),
            None => String::new(),
        };
        source.push_str(&format!(
            "def {name}(__native):\n    def {name}({names}):\n{doc}        return __native({names})\n    return {name}\n\n"
        ));
    }
    let module_code = builtins
        .getattr("compile")?
        .call1((source, FILENAME, "exec"))?;
    let mut factories = HashMap::new();
    for constant in module_code.getattr("co_consts")?.try_iter()? {
        let constant = constant?;
        if let Some(name) = constant.getattr_opt("co_name")? {
            factories.insert(name.extract::<String>()?, constant);
        }
    }

    let mut functions = Vec::with_capacity(wrappers.len());
    for wrapper in wrappers {
        let factory_code = factories
            .get(&wrapper.name)
            .ok_or_else(|| PyRuntimeError::new_err(format!("no wrapper for {}", wrapper.name)))?;
        // The functions created by the factory get its globals: the namespace
        // of the module.
        let factory = function_type.call1((factory_code, module_dict))?;
        let function = factory.call1((wrapper.native,))?;
        let defaults = wrapper
            .params
            .iter()
            .filter_map(|p| p.default.as_deref())
            .map(|default| builtins.getattr("eval")?.call1((default, module_dict)))
            .collect::<PyResult<Vec<_>>>()?;
        if !defaults.is_empty() {
            function.setattr("__defaults__", PyTuple::new(py, defaults)?)?;
        }
        function.setattr("__qualname__", &wrapper.qualname)?;
        let code = function.getattr("__code__")?;
        if code.hasattr("co_qualname")? {
            let kwargs = PyDict::new(py);
            kwargs.set_item("co_qualname", &wrapper.qualname)?;
            function.setattr("__code__", code.call_method("replace", (), Some(&kwargs))?)?;
        }
        functions.push((wrapper.name, function));
    }
    Ok(functions)
}

fn doc_of(docs: &[(&str, &'static str)], name: &str) -> Option<&'static str> {
    docs.iter().find(|(n, _)| *n == name).map(|(_, doc)| *doc)
}

/// Replace the (Rust) functions of the module by Python functions calling
/// them.
pub(crate) fn install(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    let module_dict = m.dict();
    let module_name = m.name()?;

    // The Rust functions of the module (not the ones it imported, such as
    // `pickle.loads`).
    let mut wrappers = Vec::new();
    for (name, native) in module_dict.iter() {
        if native.get_type().name()? == "builtin_function_or_method"
            && native
                .getattr(intern!(py, "__module__"))?
                .eq(&module_name)?
        {
            let name = name.cast_into::<PyString>()?.to_str()?.to_owned();
            // The signatures of the Rust functions are upstream's, except for
            // `_make_cell`, whose default value is not a literal (which a text
            // signature cannot express).
            let params = if name == "_make_cell" {
                parse_params("(value=_empty_cell_value)")?
            } else {
                parse_params(
                    &native
                        .getattr(intern!(py, "__text_signature__"))?
                        .extract::<String>()?,
                )?
            };
            wrappers.push(Wrapper {
                qualname: name.clone(),
                doc: doc_of(upstream_docs::FUNCTIONS, &name),
                name,
                params,
                native,
            });
        }
    }
    for (name, function) in build(&module_dict, wrappers)? {
        module_dict.set_item(name, function)?;
    }
    Ok(())
}

/// Replace the (Rust) methods `(name, signature)` of the `Pickler` class by
/// Python functions calling them.
pub(crate) fn install_methods(
    m: &Bound<'_, PyModule>,
    pickler: &Bound<'_, PyType>,
    methods: &[(&str, &str)],
) -> PyResult<()> {
    let mut wrappers = Vec::new();
    for (name, signature) in methods {
        let native = pickler
            .getattr(intern!(m.py(), "__dict__"))?
            .get_item(name)?;
        wrappers.push(Wrapper {
            name: name.to_string(),
            qualname: format!("Pickler.{name}"),
            params: parse_params(signature)?,
            native,
            doc: doc_of(upstream_docs::PICKLER_METHODS, name),
        });
    }
    for (name, function) in build(&m.dict(), wrappers)? {
        pickler.setattr(name.as_str(), function)?;
    }
    Ok(())
}
