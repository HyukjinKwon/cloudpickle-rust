//! `cloudpickle_rust.cloudpickle`: a Rust implementation of
//! [cloudpickle](https://github.com/cloudpipe/cloudpickle)'s
//! `cloudpickle/cloudpickle.py` module.
//!
//! The layout of the crate follows the upstream module:
//!
//! * [`lookup`]: importability checks (`_should_pickle_by_reference`,
//!   `_whichmodule`, `register_pickle_by_value`, ...) and class tracking;
//! * [`code`]: code objects introspection (`_extract_code_globals`,
//!   `_find_imported_submodules`) and `_code_reduce`;
//! * [`reducers`]: the reducers and state getters;
//! * [`reconstructors`]: the functions called at unpickling time;
//! * [`pickler`]: the `Pickler` class, `dump` and `dumps`;
//! * [`fallback`]: the call-time semantics of the globals of the module
//!   (replaced globals make the functions run upstream's code).

mod code;
mod fallback;
mod functions;
mod lookup;
mod pickler;
mod reconstructors;
mod reducers;
mod state;
mod upstream_docs;
mod utils;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule, PySet, PyString, PyTuple, PyType};

use crate::state::{Globals, Opcodes, GLOBALS};
use crate::utils::set_attr;

const MODULE_DOC: &str = r#"Pickler class to extend the standard pickle.Pickler functionality

The main objective is to make it natural to perform distributed computing on
clusters (such as PySpark, Dask, Ray...) with interactively defined code
(functions, classes, ...) written in notebooks or console.

In particular this pickler adds the following features:
- serialize interactively-defined or locally-defined functions, classes,
  enums, typevars, lambdas and nested functions to compiled byte code;
- deal with some other non-serializable objects in an ad-hoc manner where
  applicable.

This pickler is therefore meant to be used for the communication between short
lived Python processes running the same version of Python and libraries. In
particular, it is not meant to be used for long term storage of Python objects.

It does not include an unpickler, as standard Python unpickling suffices.

This module is a Rust implementation (cloudpickle-rust) of the cloudpickle
module. cloudpickle-rust is licensed under the Apache License, Version 2.0 (see
the LICENSE and NOTICE files of the distribution).

cloudpickle-rust is derived from cloudpickle, which was extracted from the
`cloud` package, developed by `PiCloud, Inc.
<https://web.archive.org/web/20140626004012/http://www.picloud.com/>`_, and is
distributed under the following license:

Copyright (c) 2012-now, CloudPickle developers and contributors.
Copyright (c) 2012, Regents of the University of California.
Copyright (c) 2009 `PiCloud, Inc. <https://web.archive.org/web/20140626004012/http://www.picloud.com/>`_.
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:
    * Redistributions of source code must retain the above copyright
      notice, this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above copyright
      notice, this list of conditions and the following disclaimer in the
      documentation and/or other materials provided with the distribution.
    * Neither the name of the University of California, Berkeley nor the
      names of its contributors may be used to endorse or promote
      products derived from this software without specific prior written
      permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED
TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
"#;

/// Upstream's definition of the sentinel class (whose instance replaces it,
/// as upstream's `@instance` decorator does).
const EMPTY_CELL_VALUE_CLASS: &std::ffi::CStr = c"class _empty_cell_value:
    \"\"\"Sentinel for empty closures.\"\"\"

    @classmethod
    def __reduce__(cls):
        return cls.__name__
";

#[pymodule(gil_used = false)]
#[pyo3(name = "cloudpickle")]
fn cloudpickle_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    if let Some(g) = GLOBALS.get(py) {
        // The extension module is being initialized again (for instance after
        // its removal from `sys.modules`): reuse the objects created by the
        // first initialization, so that the objects pickled by reference (the
        // reconstructors, the Pickler class...) keep their identity.
        let first = g.module_dict.bind(py);
        let dict = m.dict();
        for (name, value) in first.iter() {
            let skip = matches!(
                name.cast::<PyString>().ok().and_then(|n| n.to_str().ok()),
                Some("__name__" | "__loader__" | "__package__" | "__spec__" | "__file__")
            );
            if !skip {
                dict.set_item(name, value)?;
            }
        }
        return Ok(());
    }
    let globals = init_module(m)?;
    // A concurrent initialization (free-threaded build) would have produced
    // an equivalent state: keep the first one.
    let _ = GLOBALS.set(py, globals);
    Ok(())
}

fn init_module(m: &Bound<'_, PyModule>) -> PyResult<Globals> {
    let py = m.py();
    m.setattr("__doc__", MODULE_DOC)?;

    // The modules imported by upstream, which are also part of its namespace
    // (and thus, through `from .cloudpickle import *`, of the package's).
    let collections_abc = py.import("_collections_abc")?;
    let collections = py.import("collections")?;
    let abc = py.import("abc")?;
    let builtins = py.import("builtins")?;
    let copyreg = py.import("copyreg")?;
    let dataclasses = py.import("dataclasses")?;
    let dis = py.import("dis")?;
    let enum_module = py.import("enum")?;
    let io = py.import("io")?;
    let itertools = py.import("itertools")?;
    let logging = py.import("logging")?;
    let opcode = py.import("opcode")?;
    let pickle = py.import("pickle")?;
    let platform = py.import("platform")?;
    let r#struct = py.import("struct")?;
    let sys = py.import("sys")?;
    let threading = py.import("threading")?;
    let types = py.import("types")?;
    let typing = py.import("typing")?;
    let uuid = py.import("uuid")?;
    let warnings = py.import("warnings")?;
    let weakref = py.import("weakref")?;
    set_attr(m, "__builtins__", builtins.dict())?;
    set_attr(m, "_collections_abc", &collections_abc)?;
    set_attr(m, "ChainMap", collections.getattr("ChainMap")?)?;
    set_attr(m, "OrderedDict", collections.getattr("OrderedDict")?)?;
    set_attr(m, "abc", &abc)?;
    set_attr(m, "builtins", &builtins)?;
    set_attr(m, "copyreg", &copyreg)?;
    set_attr(m, "dataclasses", &dataclasses)?;
    set_attr(m, "dis", &dis)?;
    set_attr(m, "Enum", enum_module.getattr("Enum")?)?;
    set_attr(m, "io", &io)?;
    set_attr(m, "itertools", &itertools)?;
    set_attr(m, "logging", &logging)?;
    set_attr(m, "opcode", &opcode)?;
    set_attr(m, "pickle", &pickle)?;
    set_attr(m, "_pickle_getattribute", pickle.getattr("_getattribute")?)?;
    set_attr(m, "platform", &platform)?;
    set_attr(m, "struct", &r#struct)?;
    set_attr(m, "sys", &sys)?;
    set_attr(m, "threading", &threading)?;
    set_attr(m, "types", &types)?;
    set_attr(m, "typing", &typing)?;
    set_attr(m, "uuid", &uuid)?;
    set_attr(m, "warnings", &warnings)?;
    set_attr(m, "weakref", &weakref)?;

    let version_info = sys.getattr("version_info")?;
    let version: (u8, u8) = (
        version_info.get_item(0)?.extract()?,
        version_info.get_item(1)?.extract()?,
    );

    // The following import is required to be imported in the cloudpickle
    // namespace to be able to load pickle files generated with older versions of
    // cloudpickle. See: tests/test_backward_compat.py
    let cell_type = types.getattr("CellType")?;
    set_attr(m, "CellType", &cell_type)?;

    // cloudpickle is meant for inter process communication: we expect all
    // communicating processes to run the same Python version hence we favor
    // communication speed over compatibility:
    set_attr(m, "DEFAULT_PROTOCOL", pickle.getattr("HIGHEST_PROTOCOL")?)?;

    // cloudpickle-rust extension, looked up at call time like DEFAULT_PROTOCOL:
    // the filename given to the code objects pickled by value, or None to keep
    // their original filename (see code::code_reduce).
    set_attr(m, "DYNAMIC_CODE_FILENAME", "<dynamic-code>")?;

    // Names of modules whose resources should be treated as dynamic.
    let pickle_by_value_modules = PySet::empty(py)?;
    set_attr(m, "_PICKLE_BY_VALUE_MODULES", &pickle_by_value_modules)?;

    // Track the provenance of reconstructed dynamic classes to make it possible to
    // reconstruct instances from the matching singleton class definition when
    // appropriate and preserve the usual "isinstance" semantics of Python objects.
    let tracker_by_class = weakref.getattr("WeakKeyDictionary")?.call0()?;
    let tracker_by_id = weakref.getattr("WeakValueDictionary")?.call0()?;
    let tracker_lock = threading.getattr("Lock")?.call0()?;
    set_attr(m, "_DYNAMIC_CLASS_TRACKER_BY_CLASS", &tracker_by_class)?;
    set_attr(m, "_DYNAMIC_CLASS_TRACKER_BY_ID", &tracker_by_id)?;
    set_attr(m, "_DYNAMIC_CLASS_TRACKER_LOCK", &tracker_lock)?;

    let pypy = sys.getattr("implementation")?.getattr("name")?.eq("pypy")?;
    set_attr(m, "PYPY", pypy)?;

    let builtin_code_type = if pypy {
        // builtin-code objects only exist in pypy
        py.get_type::<pyo3::types::PyFloat>()
            .getattr("__new__")?
            .getattr("__code__")?
            .get_type()
            .into_any()
    } else {
        py.None().into_bound(py)
    };
    set_attr(m, "builtin_code_type", &builtin_code_type)?;

    let code_globals_cache = weakref.getattr("WeakKeyDictionary")?.call0()?;
    set_attr(m, "_extract_code_globals_cache", &code_globals_cache)?;

    lookup::register(m)?;
    code::register(m)?;

    // relevant opcodes
    let opmap = opcode.getattr("opmap")?;
    let store_global: u8 = opmap.get_item("STORE_GLOBAL")?.extract()?;
    let delete_global: Option<u8> = if version < (3, 16) {
        Some(opmap.get_item("DELETE_GLOBAL")?.extract()?)
    } else {
        None
    };
    let load_global: u8 = opmap.get_item("LOAD_GLOBAL")?.extract()?;
    let extended_arg: u8 = dis.getattr("EXTENDED_ARG")?.extract()?;
    set_attr(m, "STORE_GLOBAL", store_global)?;
    let mut global_ops = vec![store_global];
    if let Some(delete_global) = delete_global {
        set_attr(m, "DELETE_GLOBAL", delete_global)?;
        global_ops.push(delete_global);
    }
    set_attr(m, "LOAD_GLOBAL", load_global)?;
    global_ops.push(load_global);
    set_attr(m, "GLOBAL_OPS", PyTuple::new(py, global_ops)?)?;
    set_attr(m, "HAVE_ARGUMENT", dis.getattr("HAVE_ARGUMENT")?)?;
    set_attr(m, "EXTENDED_ARG", extended_arg)?;

    let builtin_type_names = PyDict::new(py);
    let type_type = py.get_type::<PyType>();
    let mut last_item = None;
    for (k, v) in types.dict().iter() {
        if v.get_type().is(&type_type) {
            builtin_type_names.set_item(&v, &k)?;
        }
        last_item = Some((k, v));
    }
    set_attr(m, "_BUILTIN_TYPE_NAMES", &builtin_type_names)?;
    // Upstream's module-level `for k, v in types.__dict__.items()` loop leaves
    // its variables in the module namespace.
    if let Some((k, v)) = last_item {
        set_attr(m, "k", k)?;
        set_attr(m, "v", v)?;
    }

    reconstructors::register(m)?;
    // `_empty_cell_value` is an instance of a Python class, like upstream's
    // (whose class statement is executed in the namespace of the module).
    py.run(EMPTY_CELL_VALUE_CLASS, Some(&m.dict()), None)?;
    let empty_cell_value = m
        .getattr("instance")?
        .call1((m.getattr("_empty_cell_value")?,))?;
    set_attr(m, "_empty_cell_value", &empty_cell_value)?;

    reducers::register(m)?;

    // COLLECTION OF DATACLASS UTILITIES
    // ---------------------------------
    // There are some internal sentinel values whose identity must be preserved when
    // unpickling dataclass fields. Each sentinel value has a unique name that we can
    // use to retrieve its identity at unpickling time.
    let dataclass_sentinels = PyDict::new(py);
    for attr in ["_FIELD", "_FIELD_CLASSVAR", "_FIELD_INITVAR"] {
        let sentinel = dataclasses.getattr(attr)?;
        dataclass_sentinels.set_item(sentinel.getattr("name")?, &sentinel)?;
    }
    set_attr(m, "_DATACLASSE_FIELD_TYPE_SENTINELS", &dataclass_sentinels)?;

    // Shorthands similar to pickle.dump/pickle.dumps
    pickler::register(m)?;

    // The functions of the module become Python functions (calling their
    // Rust implementation), like upstream's.
    functions::install(m)?;

    // set of reducers defined and used by cloudpickle (private)
    let f = |name: &str| m.getattr(name);
    let dispatch = PyDict::new(py);
    dispatch.set_item(builtins.getattr("classmethod")?, f("_classmethod_reduce")?)?;
    dispatch.set_item(io.getattr("TextIOWrapper")?, f("_file_reduce")?)?;
    dispatch.set_item(logging.getattr("Logger")?, f("_logger_reduce")?)?;
    dispatch.set_item(logging.getattr("RootLogger")?, f("_root_logger_reduce")?)?;
    dispatch.set_item(builtins.getattr("memoryview")?, f("_memoryview_reduce")?)?;
    dispatch.set_item(builtins.getattr("property")?, f("_property_reduce")?)?;
    dispatch.set_item(builtins.getattr("staticmethod")?, f("_classmethod_reduce")?)?;
    dispatch.set_item(&cell_type, f("_cell_reduce")?)?;
    dispatch.set_item(types.getattr("CodeType")?, f("_code_reduce")?)?;
    dispatch.set_item(
        types.getattr("GetSetDescriptorType")?,
        f("_getset_descriptor_reduce")?,
    )?;
    dispatch.set_item(types.getattr("ModuleType")?, f("_module_reduce")?)?;
    dispatch.set_item(types.getattr("MethodType")?, f("_method_reduce")?)?;
    dispatch.set_item(
        types.getattr("MappingProxyType")?,
        f("_mappingproxy_reduce")?,
    )?;
    dispatch.set_item(weakref.getattr("WeakSet")?, f("_weakset_reduce")?)?;
    dispatch.set_item(typing.getattr("TypeVar")?, f("_typevar_reduce")?)?;
    dispatch.set_item(
        collections_abc.getattr("dict_keys")?,
        f("_dict_keys_reduce")?,
    )?;
    dispatch.set_item(
        collections_abc.getattr("dict_values")?,
        f("_dict_values_reduce")?,
    )?;
    dispatch.set_item(
        collections_abc.getattr("dict_items")?,
        f("_dict_items_reduce")?,
    )?;
    let ordered_dict = collections.getattr("OrderedDict")?;
    let empty_odict = ordered_dict.call0()?;
    dispatch.set_item(
        empty_odict.call_method0("keys")?.get_type(),
        f("_odict_keys_reduce")?,
    )?;
    dispatch.set_item(
        empty_odict.call_method0("values")?.get_type(),
        f("_odict_values_reduce")?,
    )?;
    dispatch.set_item(
        empty_odict.call_method0("items")?.get_type(),
        f("_odict_items_reduce")?,
    )?;
    dispatch.set_item(abc.getattr("abstractmethod")?, f("_classmethod_reduce")?)?;
    dispatch.set_item(
        abc.getattr("abstractclassmethod")?,
        f("_classmethod_reduce")?,
    )?;
    dispatch.set_item(
        abc.getattr("abstractstaticmethod")?,
        f("_classmethod_reduce")?,
    )?;
    dispatch.set_item(abc.getattr("abstractproperty")?, f("_property_reduce")?)?;
    dispatch.set_item(
        dataclasses.getattr("_FIELD_BASE")?,
        f("_dataclass_field_base_reduce")?,
    )?;

    let (pickler_type, dispatch_table_type) = pickler::create_pickler_class(m, &dispatch, pypy)?;
    set_attr(m, "_DispatchTable", &dispatch_table_type)?;
    set_attr(m, "Pickler", &pickler_type)?;

    // Include pickles unloading functions in this namespace for convenience.
    set_attr(m, "load", pickle.getattr("load")?)?;
    set_attr(m, "loads", pickle.getattr("loads")?)?;

    // Backward compat alias.
    set_attr(m, "CloudPickler", &pickler_type)?;

    // The values of the globals read by the upstream functions (see
    // `fallback`): the namespace of the module is complete.
    fallback::resolve_all(py, &m.dict())?;

    let function_type = types.getattr("FunctionType")?;
    let module_name = m.name()?;
    let mut module_functions = std::collections::HashMap::new();
    for (name, value) in m.dict().iter() {
        if value.get_type().is(&function_type) && value.getattr("__module__")?.eq(&module_name)? {
            module_functions.insert(
                value.as_ptr() as usize,
                (value.unbind(), name.cast_into::<PyString>()?.unbind()),
            );
        }
    }
    let weak_key_dictionary = weakref.getattr("WeakKeyDictionary")?;

    let unbind = |name: &str| -> PyResult<Py<PyAny>> { Ok(m.getattr(name)?.unbind()) };
    Ok(Globals {
        version,
        module: m.clone().unbind(),
        module_dict: m.dict().unbind(),
        module_functions,
        pickler_function_reduce: pickler_type
            .getattr("__dict__")?
            .get_item("_function_reduce")?
            .unbind(),
        pickler_dynamic_function_reduce: pickler_type
            .getattr("__dict__")?
            .get_item("_dynamic_function_reduce")?
            .unbind(),
        pickler_function_getnewargs: pickler_type
            .getattr("__dict__")?
            .get_item("_function_getnewargs")?
            .unbind(),
        weak_key_dictionary: (
            weak_key_dictionary.clone().unbind(),
            weak_key_dictionary.getattr("get")?.unbind(),
            weak_key_dictionary.getattr("__setitem__")?.unbind(),
        ),
        pypy,
        code_exceptiontable_last: code::measure_code_exceptiontable_last(py)?,
        sys: sys.clone().unbind(),
        builtins: builtins.clone().unbind(),
        builtins_dict: builtins.dict().unbind(),
        types_module: types.clone().unbind(),
        abc_module: abc.clone().unbind(),
        logging_module: logging.clone().unbind(),
        typing_module: typing.clone().unbind(),
        uuid_module: uuid.clone().unbind(),
        weakref_module: weakref.clone().unbind(),
        pickle_module: pickle.clone().unbind(),
        function_type: types.getattr("FunctionType")?.unbind(),
        code_type: types.getattr("CodeType")?.unbind(),
        cell_type: cell_type.unbind(),
        module_type: types.getattr("ModuleType")?.unbind(),
        property_type: builtins.getattr("property")?.unbind(),
        str_type: builtins.getattr("str")?.unbind(),
        bytes_type: builtins.getattr("bytes")?.unbind(),
        none_type: py.None().bind(py).get_type().into_any().unbind(),
        ellipsis_type: py.Ellipsis().bind(py).get_type().into_any().unbind(),
        notimplemented_type: py.NotImplemented().bind(py).get_type().into_any().unbind(),
        enum_type: enum_module.getattr("Enum")?.unbind(),
        ordered_dict: ordered_dict.unbind(),
        getattr: builtins.getattr("getattr")?.unbind(),
        id: builtins.getattr("id")?.unbind(),
        pickling_error: pickle
            .getattr("PicklingError")?
            .cast_into::<PyType>()?
            .unbind(),
        struct_pack: r#struct.getattr("pack")?.unbind(),
        builtin_code_type: builtin_code_type.unbind(),
        pickle_by_value_modules: pickle_by_value_modules.unbind(),
        tracker_by_class: tracker_by_class.unbind(),
        tracker_by_id: tracker_by_id.unbind(),
        tracker_lock: tracker_lock.unbind(),
        code_globals_cache: code_globals_cache.unbind(),
        builtin_type_names: builtin_type_names.unbind(),
        dataclass_sentinels: dataclass_sentinels.unbind(),
        empty_cell_value: empty_cell_value.unbind(),
        make_function: unbind("_make_function")?,
        function_setstate: unbind("_function_setstate")?,
        make_skeleton_class: unbind("_make_skeleton_class")?,
        make_skeleton_enum: unbind("_make_skeleton_enum")?,
        class_setstate: unbind("_class_setstate")?,
        make_typevar: unbind("_make_typevar")?,
        builtin_type: unbind("_builtin_type")?,
        make_empty_cell: unbind("_make_empty_cell")?,
        make_cell: unbind("_make_cell")?,
        subimport: unbind("subimport")?,
        dynamic_subimport: unbind("dynamic_subimport")?,
        file_reconstructor: unbind("_file_reconstructor")?,
        make_dict_keys: unbind("_make_dict_keys")?,
        make_dict_values: unbind("_make_dict_values")?,
        make_dict_items: unbind("_make_dict_items")?,
        get_dataclass_field_type_sentinel: unbind("_get_dataclass_field_type_sentinel")?,
        pickler_type: pickler_type.unbind(),
        dispatch_table_type: dispatch_table_type.unbind(),
        opcodes: Opcodes {
            store_global,
            delete_global,
            load_global,
            extended_arg,
            load_global_shift: code::measure_load_global_shift(py, load_global, extended_arg)?,
        },
    })
}
