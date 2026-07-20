//! Read-only buffer-protocol access (`PyBUF_SIMPLE`).

use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::os::raw::c_int;

use crate::conversion::FromPyObject;
use crate::err::{PyErrRaised, PyResult};
use crate::ffi;
use crate::instance::Bound;
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::PyAny;

/// A read-only, C-contiguous byte view of a buffer-protocol object
/// (`bytes`, `bytearray`, `memoryview`, ...).
///
/// Requested with `PyBUF_SIMPLE`, which by definition yields a C-contiguous
/// `u8` buffer — no separate contiguity check is needed. The view holds its
/// own reference to the exporting object and releases the buffer on drop.
///
/// Note (free-threaded builds): a mutable exporter such as `bytearray` can be
/// mutated by other threads while the view is held. Reads may then see
/// inconsistent *data*, but never dangling memory — the same exposure the C
/// implementations accept.
pub struct PyBuffer<'py> {
    view: ffi::Py_buffer,
    /// Ties the view to the attached scope; `*mut ()` keeps it `!Send+!Sync`.
    _marker: PhantomData<(&'py PyAny, *mut ())>,
}

impl<'py> PyBuffer<'py> {
    /// Acquire a simple buffer view of `obj`.
    ///
    /// Raises `TypeError`/`BufferError` (from `PyObject_GetBuffer`) if `obj`
    /// does not support a simple contiguous view.
    pub fn get(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<PyBuffer<'py>> {
        let mut view = MaybeUninit::<ffi::Py_buffer>::uninit();
        let ret = unsafe {
            sys_calls::get_buffer(
                ts,
                obj.as_ptr(),
                view.as_mut_ptr(),
                ffi::PyBUF_SIMPLE as c_int,
            )
        };
        if ret != 0 {
            return Err(unsafe { PyErrRaised::assume_set(ts) });
        }
        Ok(PyBuffer {
            view: unsafe { view.assume_init() },
            _marker: PhantomData,
        })
    }

    /// The buffer contents.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        if self.view.len == 0 {
            // `buf` may be NULL for an empty buffer.
            &[]
        } else {
            // SAFETY: PyBUF_SIMPLE guarantees a C-contiguous byte buffer of
            // `len` bytes, alive until PyBuffer_Release.
            unsafe {
                std::slice::from_raw_parts(self.view.buf as *const u8, self.view.len as usize)
            }
        }
    }

    /// Length of the buffer in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.view.len as usize
    }

    /// True if the buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.view.len == 0
    }
}

impl Drop for PyBuffer<'_> {
    fn drop(&mut self) {
        // Same attachment policy as Bound/Py drops: release when attached,
        // leak (never UB) if someone smuggled the view past a detach.
        unsafe {
            if !ffi::PyThreadState_GetUnchecked().is_null() {
                // tls-fallback: Drop has nowhere to store a token borrow.
                ffi::PyBuffer_Release(&mut self.view);
            } else {
                debug_assert!(
                    false,
                    "PyBuffer dropped while thread state is detached; leaking"
                );
            }
        }
    }
}

impl<'py> FromPyObject<'py> for PyBuffer<'py> {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
        PyBuffer::get(ts, obj)
    }
}
