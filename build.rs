fn main() {
    // Expose PyO3's interpreter configuration flags (e.g. `cfg(PyPy)`) to
    // this crate.
    pyo3_build_config::use_pyo3_cfgs();
}
