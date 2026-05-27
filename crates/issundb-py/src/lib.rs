#[cfg(feature = "extension-module")]
mod bindings;

#[cfg(feature = "extension-module")]
use pyo3::prelude::*;

#[cfg(feature = "extension-module")]
#[pymodule]
fn issundb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<bindings::PyGraph>()?;
    Ok(())
}
