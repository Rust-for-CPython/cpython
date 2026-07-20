//! The module type.

use std::ffi::CStr;

use crate::conversion::IntoPyObject;
use crate::err::{PyResult, error_on_minus_one};
use crate::ffi;
use crate::instance::Bound;
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::any::PyAny;
use crate::types::type_object::PyType;

/// The Python module type marker.
#[repr(transparent)]
pub struct PyModule(PyAny);

impl<'py> Bound<'py, PyModule> {
    /// Add an attribute to the module (constants, exception types, ...).
    pub fn add(
        &self,
        ts: &ThreadState<'py>,
        name: &CStr,
        value: impl IntoPyObject<'py>,
    ) -> PyResult<()> {
        let value = value.into_pyobject(ts)?;
        let ret = unsafe {
            sys_calls::module_add_object_ref(ts, self.as_ptr(), name.as_ptr(), value.as_ptr())
        };
        // `value` drops here, releasing our reference; AddObjectRef took its
        // own.
        error_on_minus_one(ts, ret)
    }

    /// Add a type object to the module under its (unqualified) name.
    pub fn add_type(&self, ts: &ThreadState<'py>, ty: &Bound<'py, PyType>) -> PyResult<()> {
        let ret = unsafe {
            sys_calls::module_add_type(ts, self.as_ptr(), ty.as_ptr() as *mut ffi::PyTypeObject)
        };
        error_on_minus_one(ts, ret)
    }
}
