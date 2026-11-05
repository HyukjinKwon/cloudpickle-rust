"""Benchmark the pickling speed of cloudpickle-rust and upstream cloudpickle.

Usage (with both cloudpickle-rust, built in release mode, and cloudpickle
installed):

    python benchmarks/bench.py

Each case is timed alternately with both libraries (best of 15 runs). For the
payloads that the standard library can pickle in the same way, the time of
plain `pickle.dumps` is reported too, as the lower bound.
"""

import dataclasses
import enum
import gc
import pickle
import sys
import timeit
import typing

import cloudpickle
import cloudpickle_rust

GLOBAL = 1
OTHER_GLOBAL = "x"


def make_closure():
    a = 1
    b = [1, 2, 3]

    def f(x):
        return x + a + len(b) + GLOBAL + len(OTHER_GLOBAL)

    return f


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def norm(self):
        return (self.x**2 + self.y**2) ** 0.5


@dataclasses.dataclass
class Record:
    a: int
    b: str
    c: float


class Color(enum.Enum):
    RED = 1
    GREEN = 2


T = typing.TypeVar("T")


class Box(typing.Generic[T]):
    def __init__(self, item: T):
        self.item = item

    def get(self) -> T:
        return self.item


# (name, factory, whether plain pickle pickles the same content)
CASES = [
    ("lambda", lambda: (lambda x: x + GLOBAL), False),
    ("closure with globals", make_closure, False),
    ("dynamic class", lambda: Point, False),
    ("dynamic dataclass", lambda: Record, False),
    ("dynamic enum", lambda: Color, False),
    ("generic class", lambda: Box, False),
    (
        "1k distinct lambdas",
        lambda: [eval("lambda x: x + GLOBAL") for _ in range(1000)],
        False,
    ),
    (
        "100k instances of a dynamic class",
        lambda: [Point(i, i) for i in range(100_000)],
        True,
    ),
    (
        "100k dataclass instances",
        lambda: [Record(i, str(i), float(i)) for i in range(100_000)],
        True,
    ),
    (
        "100k builtins (dict of lists)",
        lambda: {str(i): [i, float(i), str(i)] for i in range(100_000)},
        True,
    ),
]


def interleaved(funcs, number, repeat=15):
    """Best time of each function, measured alternately (robust to noise)."""
    best = [float("inf")] * len(funcs)
    for _ in range(repeat):
        for i, func in enumerate(funcs):
            gc.collect()
            best[i] = min(best[i], timeit.Timer(func).timeit(number=number) / number)
    return best


def main():
    print(f"Python {sys.version.split()[0]} ({sys.implementation.name})")
    print(f"{'case':38} {'cloudpickle':>13} {'cloudpickle_rust':>17} {'speedup':>8}")
    for name, factory, with_pickle in CASES:
        obj = factory()
        # Same payload modulo the module name / class tracker ids.
        a, b = cloudpickle.dumps(obj), cloudpickle_rust.dumps(obj)
        assert abs(len(a) - len(b)) < max(64, len(a) // 10), (name, len(a), len(b))
        once = timeit.Timer(lambda: cloudpickle.dumps(obj)).timeit(number=1)
        number = max(1, int(0.1 / max(once, 1e-7)))
        funcs = [lambda: cloudpickle.dumps(obj), lambda: cloudpickle_rust.dumps(obj)]
        if with_pickle:
            funcs.append(lambda: pickle.dumps(obj, protocol=5))
        times = interleaved(funcs, number)
        t_up, t_rs = times[:2]
        baseline = f"   (pickle: {times[2] * 1e6:.1f}us)" if len(times) == 3 else ""
        print(
            f"{name:38} {t_up * 1e6:11.1f}us {t_rs * 1e6:15.1f}us {t_up / t_rs:7.2f}x{baseline}"
        )


if __name__ == "__main__":
    main()
