//! The `bytes` type.

use std::mem::MaybeUninit;
use std::os::raw::c_char;
use std::ptr::NonNull;

use crate::err::{PyErrRaised, PyMemoryError, PyResult};
use crate::ffi;
use crate::instance::Bound;
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::any::PyAny;

/// The Python `bytes` type marker.
#[repr(transparent)]
pub struct PyBytes(PyAny);

impl PyBytes {
    /// Create a `bytes` object as a copy of `data`.
    pub fn new<'py>(ts: &ThreadState<'py>, data: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
        let ptr = unsafe {
            sys_calls::bytes_from_string_and_size(
                ts,
                data.as_ptr() as *const c_char,
                data.len() as ffi::Py_ssize_t,
            )
        };
        match NonNull::new(ptr) {
            Some(p) => Ok(unsafe { Bound::from_owned_ptr(ts, p) }),
            None => Err(unsafe { PyErrRaised::assume_set(ts) }),
        }
    }

    /// Create a `bytes` object of length `len` and let `init` fill it,
    /// avoiding the extra copy of building in a `Vec` first.
    ///
    /// `init` receives the uninitialized buffer and must fully initialize it
    /// when returning `Ok(())`.
    pub fn new_with<'py>(
        ts: &ThreadState<'py>,
        len: usize,
        init: impl FnOnce(&mut [MaybeUninit<u8>]) -> PyResult<()>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        if len > ffi::Py_ssize_t::MAX as usize {
            return Err(PyMemoryError::raise(ts, "bytes object is too large"));
        }
        let ptr = unsafe {
            sys_calls::bytes_from_string_and_size(ts, std::ptr::null(), len as ffi::Py_ssize_t)
        };
        let Some(p) = NonNull::new(ptr) else {
            return Err(unsafe { PyErrRaised::assume_set(ts) });
        };
        let bytes: Bound<'py, PyBytes> = unsafe { Bound::from_owned_ptr(ts, p) };
        let buf = unsafe {
            let data = sys_calls::bytes_as_string(ts, bytes.as_ptr()) as *mut MaybeUninit<u8>;
            std::slice::from_raw_parts_mut(data, len)
        };
        init(buf)?;
        Ok(bytes)
    }
}
