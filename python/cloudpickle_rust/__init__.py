from . import cloudpickle
from .cloudpickle import *  # noqa

# An extension, looked up in cloudpickle_rust.cloudpickle where it has to be set:
# a copy of it here would have no effect.
del DYNAMIC_CODE_FILENAME  # noqa: F821

__doc__ = cloudpickle.__doc__

__version__ = "0.1.0"

__all__ = [  # noqa
    "__version__",
    "Pickler",
    "CloudPickler",
    "dumps",
    "loads",
    "dump",
    "load",
    "register_pickle_by_value",
    "unregister_pickle_by_value",
]
