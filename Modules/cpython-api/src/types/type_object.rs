//! The type type.

use std::ffi::CStr;
use std::ptr::NonNull;

use crate::err::{PyErrRaised, PyResult};
use crate::instance::Bound;
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::any::PyAny;

/// The Python type-object marker.
#[repr(transparent)]
pub struct PyType(PyAny);

impl PyType {
    /// Create a new exception type (`PyErr_NewException`).
    ///
    /// `qualname` must be of the form `"module.name"` (e.g. `c"zlib.error"`).
    /// With `base: None` the base class is `Exception`.
    ///
    /// Exception types belong in per-module state, created from the module's
    /// exec function — never in statics — so that modules stay subinterpreter
    /// compatible.
    pub fn new_exception<'py>(
        ts: &ThreadState<'py>,
        qualname: &CStr,
        base: Option<&Bound<'py, PyType>>,
    ) -> PyResult<Bound<'py, PyType>> {
        let base_ptr = base.map_or(std::ptr::null_mut(), |b| b.as_ptr());
        let ptr = unsafe { sys_calls::err_new_exception(ts, qualname.as_ptr(), base_ptr) };
        match NonNull::new(ptr) {
            Some(p) => Ok(unsafe { Bound::from_owned_ptr(ts, p) }),
            None => Err(unsafe { PyErrRaised::assume_set(ts) }),
        }
    }
}
