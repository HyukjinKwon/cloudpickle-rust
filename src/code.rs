//! Code object introspection: globals referenced by a code object, imported
//! submodules used by a function, and the reducer of code objects.

use std::collections::HashSet;

use pyo3::exceptions::{PyIndexError, PyTypeError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{
    PyBytes, PyCFunction, PyDict, PyList, PyModule, PyString, PyTuple, PyWeakrefReference,
};

use crate::state::{globals, GLOBALS};
use crate::utils::{add_function, fresh_str, module_global, py_id};

/// Names referenced by the global-referencing instructions of `code`
/// (`LOAD_GLOBAL`, `STORE_GLOBAL` and `DELETE_GLOBAL`), in bytecode order.
///
/// Equivalent to `[instr.argval for instr in dis.get_instructions(code) if
/// instr.opcode in GLOBAL_OPS]` but works directly on the raw bytecode.
/// Inline cache entries (Python 3.11+) are zeroed in `co_code`, i.e. decode
/// as harmless `CACHE` instructions without argument.
pub(crate) fn walk_global_ops<'py>(code: &Bound<'py, PyAny>) -> PyResult<Vec<Bound<'py, PyAny>>> {
    let py = code.py();
    let ops = &globals(py).opcodes;
    let Some(load_global_shift) = ops.load_global_shift else {
        return walk_global_ops_with_dis(code);
    };
    let co_code = code.getattr(intern!(py, "co_code"))?;
    let co_code = co_code.cast::<PyBytes>()?;
    let co_names = code.getattr(intern!(py, "co_names"))?;
    let co_names = co_names.cast::<PyTuple>()?;
    let bytecode = co_code.as_bytes();

    let mut names = Vec::new();
    let mut extended_arg: u64 = 0;
    for unit in bytecode.chunks_exact(2) {
        let (op, arg) = (unit[0], (unit[1] as u64 | extended_arg) & 0xFFFF_FFFF);
        if op == ops.extended_arg {
            extended_arg = arg << 8;
            continue;
        }
        extended_arg = 0;
        let namei = if op == ops.load_global {
            arg >> load_global_shift
        } else if op == ops.store_global || Some(op) == ops.delete_global {
            arg
        } else {
            continue;
        };
        let name = co_names
            .get_item(namei as usize)
            .map_err(|_| PyIndexError::new_err("tuple index out of range"))?;
        names.push(name);
    }
    Ok(names)
}

/// Upstream's implementation, for interpreters whose bytecode encoding of the
/// global-referencing instructions is not the expected one.
fn walk_global_ops_with_dis<'py>(code: &Bound<'py, PyAny>) -> PyResult<Vec<Bound<'py, PyAny>>> {
    let py = code.py();
    let ops = &globals(py).opcodes;
    let mut names = Vec::new();
    let instructions = py
        .import("dis")?
        .call_method1("get_instructions", (code,))?;
    for instr in instructions.try_iter()? {
        let instr = instr?;
        let op: u8 = instr.getattr(intern!(py, "opcode"))?.extract()?;
        if op == ops.load_global || op == ops.store_global || Some(op) == ops.delete_global {
            names.push(instr.getattr(intern!(py, "argval"))?);
        }
    }
    Ok(names)
}

/// Measure the encoding of the oparg of `LOAD_GLOBAL` (see
/// [`crate::state::Opcodes::load_global_shift`]) on a probe function
/// referencing two globals: `co_names` is `(a, b)`, so the oparg of the second
/// `LOAD_GLOBAL` is `1 << shift` (possibly with the low "push NULL" bit set).
pub(crate) fn measure_load_global_shift(
    py: Python<'_>,
    load_global: u8,
    extended_arg: u8,
) -> PyResult<Option<u32>> {
    let source =
        "def probe():\n    return (_cloudpickle_rust_probe_a, _cloudpickle_rust_probe_b)\n";
    let module_code = py.import("builtins")?.getattr("compile")?.call1((
        source,
        "<cloudpickle_rust probe>",
        "exec",
    ))?;
    let code_type = py.import("types")?.getattr("CodeType")?;
    let mut probe = None;
    for constant in module_code.getattr("co_consts")?.try_iter()? {
        let constant = constant?;
        if constant.is_instance(&code_type)? {
            probe = Some(constant);
        }
    }
    let Some(probe) = probe else { return Ok(None) };
    let co_code = probe.getattr("co_code")?;
    let bytecode = co_code.cast::<PyBytes>()?.as_bytes();
    let mut opargs = Vec::new();
    let mut extended: u64 = 0;
    for unit in bytecode.chunks_exact(2) {
        let arg = unit[1] as u64 | extended;
        if unit[0] == extended_arg {
            extended = arg << 8;
            continue;
        }
        extended = 0;
        if unit[0] == load_global {
            opargs.push(arg);
        }
    }
    Ok(match opargs.as_slice() {
        [0 | 1, 1] => Some(0),
        [0 | 1, 2 | 3] => Some(1),
        _ => None,
    })
}

/// Measure the order of the arguments of the `types.CodeType` constructor
/// (see [`crate::state::Globals::code_exceptiontable_last`]): rebuild the
/// code object of a probe closure (with free variables, so that a wrong order
/// cannot produce an equal code object) with both orders.
pub(crate) fn measure_code_exceptiontable_last<'py>(py: Python<'py>) -> PyResult<bool> {
    let source = "def outer():\n    x = 1\n    def probe():\n        return x\n    return probe\n";
    let module_code = py.import("builtins")?.getattr("compile")?.call1((
        source,
        "<cloudpickle_rust probe>",
        "exec",
    ))?;
    let code_type = py.import("types")?.getattr("CodeType")?;
    let find_code = |code: &Bound<'py, PyAny>| -> PyResult<Option<Bound<'py, PyAny>>> {
        for constant in code.getattr("co_consts")?.try_iter()? {
            let constant = constant?;
            if constant.is_instance(&code_type)? {
                return Ok(Some(constant));
            }
        }
        Ok(None)
    };
    let Some(outer) = find_code(&module_code)? else {
        return Ok(false);
    };
    let Some(probe) = find_code(&outer)? else {
        return Ok(false);
    };
    if !probe.hasattr("co_exceptiontable")? {
        return Ok(false);
    }
    let mut args = Vec::new();
    for name in [
        "co_argcount",
        "co_posonlyargcount",
        "co_kwonlyargcount",
        "co_nlocals",
        "co_stacksize",
        "co_flags",
        "co_code",
        "co_consts",
        "co_names",
        "co_varnames",
        "co_filename",
        "co_name",
        "co_qualname",
        "co_firstlineno",
        "co_linetable",
    ] {
        args.push(probe.getattr(name)?);
    }
    let rebuilds = |tail: [&str; 3]| -> PyResult<bool> {
        let mut all_args = args.clone();
        for name in tail {
            all_args.push(probe.getattr(name)?);
        }
        Ok(match code_type.call1(PyTuple::new(py, all_args)?) {
            Ok(code) => code.eq(&probe)?,
            Err(_) => false,
        })
    };
    if rebuilds(["co_exceptiontable", "co_freevars", "co_cellvars"])? {
        return Ok(false);
    }
    rebuilds(["co_freevars", "co_cellvars", "co_exceptiontable"])
}

#[pyfunction]
#[pyo3(name = "_walk_global_ops")]
fn py_walk_global_ops<'py>(code: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    // Upstream returns a generator: return an iterator for consistency.
    let names = PyList::new(code.py(), walk_global_ops(code)?)?;
    Ok(names.as_any().try_iter()?.into_any())
}

/// Find all globals names read or written to by codeblock co.
///
/// The results are cached per code object (by identity) in
/// `_extract_code_globals_cache`, a dict mapping `id(code)` to a
/// `(weakref(code), names)` pair. The weakref callback evicts the entry when
/// the code object is garbage collected.
pub(crate) fn extract_code_globals<'py>(co: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let py = co.py();
    let g = globals(py);
    let cache = g.code_globals_cache.bind(py);
    let key = py_id(co)?;
    if let Some(entry) = cache.get_item(&key)? {
        if let Ok(entry) = entry.cast::<PyTuple>() {
            let referent = entry.get_item(0)?.call0()?;
            if referent.is(co) {
                return Ok(entry.get_item(1)?.cast_into::<PyDict>()?);
            }
        }
    }

    // We use a dict with None values instead of a set to get a
    // deterministic order and avoid introducing non-deterministic pickle
    // bytes as a results.
    let out_names = PyDict::new(py);
    for name in walk_global_ops(co)? {
        out_names.set_item(name, py.None())?;
    }

    // Declaring a function inside another one using the "def ..." syntax
    // generates a constant code object corresponding to the one of the
    // nested function's As the nested function may itself need global
    // variables, we need to introspect its code, extract its globals, (look
    // for code object in it's co_consts attribute..) and add the result to
    // code_globals
    let co_consts = co.getattr(intern!(py, "co_consts"))?;
    if co_consts.is_truthy()? {
        let code_type = g.code_type.bind(py);
        for constant in co_consts.try_iter()? {
            let constant = constant?;
            if constant.is_instance(code_type)? {
                out_names.update(extract_code_globals(&constant)?.as_mapping())?;
            }
        }
    }

    let callback_key = key.clone().unbind();
    let callback = PyCFunction::new_closure(
        py,
        Some(c"_evict_code_globals"),
        None,
        move |args, _kwargs| -> PyResult<()> {
            let py = args.py();
            if let Some(g) = GLOBALS.get(py) {
                let cache = g.code_globals_cache.bind(py);
                let key = callback_key.bind(py);
                let dead_ref = args.get_item(0)?;
                if let Some(entry) = cache.get_item(key)? {
                    // Only evict the entry if it was created for the dead object.
                    if entry.cast::<PyTuple>()?.get_item(0)?.is(&dead_ref) {
                        cache.del_item(key)?;
                    }
                }
            }
            Ok(())
        },
    )?;
    let weak = PyWeakrefReference::new_with(co, callback)?;
    cache.set_item(key, (weak, &out_names))?;
    Ok(out_names)
}

#[pyfunction]
#[pyo3(name = "_extract_code_globals")]
fn py_extract_code_globals<'py>(co: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    extract_code_globals(co)
}

/// Find currently imported submodules used by a function.
///
/// Submodules used by a function need to be detected and referenced for the
/// function to work correctly at depickling time. Because submodules can be
/// referenced as attribute of their parent package (``package.submodule``), we
/// need a special introspection technique that does not rely on GLOBAL-related
/// opcodes to find references of them in a code object.
///
/// Example:
/// ```text
/// import concurrent.futures
/// import cloudpickle
/// def func():
///     x = concurrent.futures.ThreadPoolExecutor
/// if __name__ == '__main__':
///     cloudpickle.dumps(func)
/// ```
/// The globals extracted by cloudpickle in the function's state include the
/// concurrent package, but not its submodule (here, concurrent.futures), which
/// is the module used by func. Find_imported_submodules will detect the usage
/// of concurrent.futures. Saving this module alongside with func will ensure
/// that calling func once depickled does not fail due to concurrent.futures
/// not being imported
pub(crate) fn find_imported_submodules<'py>(
    code: &Bound<'py, PyAny>,
    top_level_dependencies: impl IntoIterator<Item = PyResult<Bound<'py, PyAny>>>,
) -> PyResult<Bound<'py, PyList>> {
    let py = code.py();
    let g = globals(py);
    let module_type = g.module_type.bind(py);
    let subimports = PyList::empty(py);
    // `set(code.co_names)`, computed lazily.
    let mut co_names: Option<CoNames<'py>> = None;

    // check if any known dependency is an imported package
    for x in top_level_dependencies {
        let x = x?;
        if !x.is_instance(module_type)? {
            continue;
        }
        match x.getattr_opt(intern!(py, "__package__"))? {
            Some(package) if package.is_truthy()? => {}
            _ => continue,
        }
        // check if the package has any currently loaded sub-imports
        let x_name = x.getattr(intern!(py, "__name__"))?;
        let prefix = x_name.add(".")?;
        let prefix_str = prefix
            .cast::<PyString>()
            .ok()
            .and_then(|p| p.to_str().ok().map(str::to_owned));

        // A concurrent thread could mutate sys.modules,
        // make sure we iterate over a copy to avoid exceptions
        let sys_modules = g.sys.bind(py).getattr(intern!(py, "modules"))?;
        let names = py
            .get_type::<PyList>()
            .call1((&sys_modules,))?
            .cast_into::<PyList>()?;
        for name in names.iter() {
            // Older versions of pytest will add a "None" module to
            // sys.modules.
            if name.is_none() {
                continue;
            }
            let co_names = match &co_names {
                Some(c) => c,
                None => co_names.insert(CoNames::new(code)?),
            };
            let fast = match (&prefix_str, name.cast_exact::<PyString>()) {
                (Some(prefix), Ok(name)) => name.to_str().ok().map(|name| {
                    name.strip_prefix(prefix.as_str())
                        .map(|rest| rest.split('.').all(|token| co_names.contains_str(token)))
                }),
                _ => None,
            };
            let addressable = match fast {
                Some(Some(addressable)) => addressable,
                Some(None) => false,
                // Generic path, mirroring the Python code.
                None => {
                    if !name
                        .call_method1(intern!(py, "startswith"), (&prefix,))?
                        .is_truthy()?
                    {
                        continue;
                    }
                    let rest = name.get_item(pyo3::types::PySlice::new(
                        py,
                        prefix.len()? as isize,
                        isize::MAX,
                        1,
                    ))?;
                    let mut addressable = true;
                    for token in rest
                        .call_method1(intern!(py, "split"), (".",))?
                        .try_iter()?
                    {
                        if !co_names.contains(&token?)? {
                            addressable = false;
                            break;
                        }
                    }
                    addressable
                }
            };
            // check whether the function can address the sub-module
            if addressable {
                subimports.append(sys_modules.get_item(&name)?)?;
            }
        }
    }
    Ok(subimports)
}

/// `set(code.co_names)`
struct CoNames<'py> {
    strings: HashSet<String>,
    set: Bound<'py, pyo3::types::PySet>,
}

impl<'py> CoNames<'py> {
    fn new(code: &Bound<'py, PyAny>) -> PyResult<Self> {
        let py = code.py();
        let co_names = code.getattr(intern!(py, "co_names"))?;
        let set = pyo3::types::PySet::new(py, co_names.try_iter()?.collect::<PyResult<Vec<_>>>()?)?;
        let mut strings = HashSet::new();
        for name in set.iter() {
            if let Ok(name) = name.cast_exact::<PyString>() {
                if let Ok(name) = name.to_str() {
                    strings.insert(name.to_owned());
                }
            }
        }
        Ok(Self { strings, set })
    }

    fn contains_str(&self, token: &str) -> bool {
        self.strings.contains(token)
    }

    fn contains(&self, token: &Bound<'py, PyAny>) -> PyResult<bool> {
        self.set.contains(token)
    }
}

#[pyfunction]
#[pyo3(name = "_find_imported_submodules")]
fn py_find_imported_submodules<'py>(
    code: &Bound<'py, PyAny>,
    top_level_dependencies: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyList>> {
    find_imported_submodules(code, top_level_dependencies.try_iter()?)
}

/// The `co_filename` given to a code object pickled by value.
///
/// cloudpickle-rust extension: the `DYNAMIC_CODE_FILENAME` module attribute,
/// looked up at call time like `DEFAULT_PROTOCOL`, is `"<dynamic-code>"` by
/// default as upstream. Setting it to `None` keeps the original filename, as
/// cloudpickle 3.1 and earlier did, so that the tracebacks and the profilers of
/// the processes that load the code can show where it was defined.
fn dynamic_code_filename<'py>(obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = obj.py();
    let filename = module_global(py, intern!(py, "DYNAMIC_CODE_FILENAME"))?;
    if filename.is_none() {
        // The original object, as cloudpickle 3.1: code objects defined in the
        // same file share (and memoize) their filename.
        obj.getattr(intern!(py, "co_filename"))
    } else if filename.is_instance_of::<PyString>() {
        // A new copy for each code object, as `"".join("<dynamic-code>")`.
        fresh_str(&filename)
    } else {
        Err(PyTypeError::new_err(format!(
            "DYNAMIC_CODE_FILENAME must be a str or None, not {}",
            filename.get_type().name()?
        )))
    }
}

/// code object reducer.
pub(crate) fn code_reduce<'py>(obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    // If you are not sure about the order of arguments, take a look at help
    // of the specific type from types, for example:
    // >>> from types import CodeType
    // >>> help(CodeType)
    let py = obj.py();
    let g = globals(py);
    let attr = |name: &Bound<'py, PyString>| obj.getattr(name);

    // Hack to circumvent non-predictable memoization caused by string interning.
    // See the inline comment in _class_setstate for details.
    let co_name = fresh_str(&attr(intern!(py, "co_name"))?)?;

    // co_filename is not used in the constructor of code objects, so we can
    // safely set it to indicate that this is dynamic code. This also makes
    // the payload deterministic, independent of where the function is defined
    // which is especially useful when defining classes in jupyter/ipython
    // cells which do not have a deterministic filename.
    let co_filename = dynamic_code_filename(obj)?;

    // Create shallow copies of these tuple to make cloudpickle payload deterministic.
    // When creating a code object during load, copies of these four tuples are
    // created, while in the main process, these tuples can be shared.
    // By always creating copies, we make sure the resulting payload is deterministic.
    let copy = |name: &Bound<'py, PyString>| -> PyResult<Bound<'py, PyAny>> {
        let items = attr(name)?.try_iter()?.collect::<PyResult<Vec<_>>>()?;
        Ok(PyTuple::new(py, items)?.into_any())
    };
    let co_names = copy(intern!(py, "co_names"))?;
    let co_varnames = copy(intern!(py, "co_varnames"))?;
    let co_freevars = copy(intern!(py, "co_freevars"))?;
    let co_cellvars = copy(intern!(py, "co_cellvars"))?;

    let args: Vec<Bound<'py, PyAny>> =
        if g.code_exceptiontable_last && obj.hasattr(intern!(py, "co_exceptiontable"))? {
            // PyPy 7.3 for Python 3.11: same attributes as CPython 3.11, but the
            // exception table comes after the free and cell variables in the
            // signature of the constructor:
            // (..., firstlineno, linetable, freevars, cellvars, exceptiontable)
            // Upstream uses the CPython order, which this PyPy cannot load.
            vec![
                attr(intern!(py, "co_argcount"))?,
                attr(intern!(py, "co_posonlyargcount"))?,
                attr(intern!(py, "co_kwonlyargcount"))?,
                attr(intern!(py, "co_nlocals"))?,
                attr(intern!(py, "co_stacksize"))?,
                attr(intern!(py, "co_flags"))?,
                attr(intern!(py, "co_code"))?,
                attr(intern!(py, "co_consts"))?,
                co_names,
                co_varnames,
                co_filename,
                co_name,
                attr(intern!(py, "co_qualname"))?,
                attr(intern!(py, "co_firstlineno"))?,
                attr(intern!(py, "co_linetable"))?,
                co_freevars,
                co_cellvars,
                attr(intern!(py, "co_exceptiontable"))?,
            ]
        } else if obj.hasattr(intern!(py, "co_exceptiontable"))? {
            // Python 3.11 and later: there are some new attributes
            // related to the enhanced exceptions.
            vec![
                attr(intern!(py, "co_argcount"))?,
                attr(intern!(py, "co_posonlyargcount"))?,
                attr(intern!(py, "co_kwonlyargcount"))?,
                attr(intern!(py, "co_nlocals"))?,
                attr(intern!(py, "co_stacksize"))?,
                attr(intern!(py, "co_flags"))?,
                attr(intern!(py, "co_code"))?,
                attr(intern!(py, "co_consts"))?,
                co_names,
                co_varnames,
                co_filename,
                co_name,
                attr(intern!(py, "co_qualname"))?,
                attr(intern!(py, "co_firstlineno"))?,
                attr(intern!(py, "co_linetable"))?,
                attr(intern!(py, "co_exceptiontable"))?,
                co_freevars,
                co_cellvars,
            ]
        } else if obj.hasattr(intern!(py, "co_linetable"))? {
            // Python 3.10 and later: obj.co_lnotab is deprecated and constructor
            // expects obj.co_linetable instead.
            vec![
                attr(intern!(py, "co_argcount"))?,
                attr(intern!(py, "co_posonlyargcount"))?,
                attr(intern!(py, "co_kwonlyargcount"))?,
                attr(intern!(py, "co_nlocals"))?,
                attr(intern!(py, "co_stacksize"))?,
                attr(intern!(py, "co_flags"))?,
                attr(intern!(py, "co_code"))?,
                attr(intern!(py, "co_consts"))?,
                co_names,
                co_varnames,
                co_filename,
                co_name,
                attr(intern!(py, "co_firstlineno"))?,
                attr(intern!(py, "co_linetable"))?,
                co_freevars,
                co_cellvars,
            ]
        } else if obj.hasattr(intern!(py, "co_nmeta"))? {
            // "nogil" Python: modified attributes from 3.9
            vec![
                attr(intern!(py, "co_argcount"))?,
                attr(intern!(py, "co_posonlyargcount"))?,
                attr(intern!(py, "co_kwonlyargcount"))?,
                attr(intern!(py, "co_nlocals"))?,
                attr(intern!(py, "co_framesize"))?,
                attr(intern!(py, "co_ndefaultargs"))?,
                attr(intern!(py, "co_nmeta"))?,
                attr(intern!(py, "co_flags"))?,
                attr(intern!(py, "co_code"))?,
                attr(intern!(py, "co_consts"))?,
                co_varnames,
                co_filename,
                co_name,
                attr(intern!(py, "co_firstlineno"))?,
                attr(intern!(py, "co_lnotab"))?,
                attr(intern!(py, "co_exc_handlers"))?,
                attr(intern!(py, "co_jump_table"))?,
                co_freevars,
                co_cellvars,
                attr(intern!(py, "co_free2reg"))?,
                attr(intern!(py, "co_cell2reg"))?,
            ]
        } else {
            // Backward compat for 3.8 and 3.9
            vec![
                attr(intern!(py, "co_argcount"))?,
                attr(intern!(py, "co_posonlyargcount"))?,
                attr(intern!(py, "co_kwonlyargcount"))?,
                attr(intern!(py, "co_nlocals"))?,
                attr(intern!(py, "co_stacksize"))?,
                attr(intern!(py, "co_flags"))?,
                attr(intern!(py, "co_code"))?,
                attr(intern!(py, "co_consts"))?,
                co_names,
                co_varnames,
                co_filename,
                co_name,
                attr(intern!(py, "co_firstlineno"))?,
                attr(intern!(py, "co_lnotab"))?,
                co_freevars,
                co_cellvars,
            ]
        };
    Ok((g.code_type.bind(py), PyTuple::new(py, args)?)
        .into_pyobject(py)?
        .into_any())
}

#[pyfunction]
#[pyo3(name = "_code_reduce")]
fn py_code_reduce<'py>(obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    code_reduce(obj)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    add_function(m, wrap_pyfunction!(py_extract_code_globals, m)?)?;
    add_function(m, wrap_pyfunction!(py_find_imported_submodules, m)?)?;
    add_function(m, wrap_pyfunction!(py_walk_global_ops, m)?)?;
    add_function(m, wrap_pyfunction!(py_code_reduce, m)?)?;
    Ok(())
}
