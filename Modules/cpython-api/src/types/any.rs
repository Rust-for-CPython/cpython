//! The abstract object type.

use crate::ffi;

/// Any Python object.
///
/// A Python object can be mutated by other code at any time, so there is
/// never a `&mut PyAny`; the wrapped `PyObject` already models interior
/// mutability (`cpython-sys` defines it as a transparent `UnsafeCell`).
///
/// This type is only ever used behind [`crate::Bound`]/[`crate::Py`] (or
/// `&PyAny`); it is never instantiated by value.
#[repr(transparent)]
pub struct PyAny(ffi::PyObject);

impl PyAny {
    /// The raw object pointer.
    #[inline]
    pub fn as_ptr(&self) -> *mut ffi::PyObject {
        self as *const PyAny as *mut ffi::PyObject
    }
}
