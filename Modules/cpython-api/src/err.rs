//! Error handling: the zero-sized "exception is set" marker and raise helpers.
//!
//! The exception itself lives where C puts it — in the thread state. Rust
//! code only carries the [`PyErrRaised`] marker, so `Result<T, PyErrRaised>`
//! is the same size as `T` and there is no duplicate error state to
//! reconcile.

use std::ffi::CString;
use std::fmt;
use std::marker::PhantomData;

use crate::Py;
use crate::ffi;
use crate::instance::Bound;
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::PyType;

/// Zero-sized marker: a Python exception has been set on the current thread.
///
/// Produced by the `raise` methods on exception types; consumed by returning
/// it (usually with `?`) until a trampoline turns it into a `NULL` return.
#[must_use = "a raised exception must be propagated (usually with `?` or `return Err(..)`)"]
pub struct PyErrRaised {
    /// `*mut ()` keeps the marker `!Send`: it refers to per-thread state.
    _priv: PhantomData<*mut ()>,
}

/// The result of any fallible operation against the interpreter.
pub type PyResult<T> = Result<T, PyErrRaised>;

impl PyErrRaised {
    /// Assert that an exception is already set (e.g. after an FFI call
    /// returned `NULL`/`-1`).
    ///
    /// # Safety
    ///
    /// An exception must actually be set on this thread; the marker's whole
    /// meaning depends on it.
    #[inline]
    pub unsafe fn assume_set(ts: &ThreadState<'_>) -> PyErrRaised {
        debug_assert!(
            ts.exception_set(),
            "PyErrRaised::assume_set called with no exception set"
        );
        PyErrRaised { _priv: PhantomData }
    }
}

impl fmt::Debug for PyErrRaised {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PyErrRaised")
    }
}

/// Set `exc_type` with a formatted message and return the raised marker.
pub(crate) fn raise_with_type(
    ts: &ThreadState<'_>,
    exc_type: *mut ffi::PyObject,
    msg: impl fmt::Display,
) -> PyErrRaised {
    let text = msg.to_string();
    // A message containing an interior NUL can't round-trip through the C
    // API; truncate at the first NUL rather than losing the exception.
    let ctext = CString::new(text.clone())
        .unwrap_or_else(|e| CString::new(&text[..e.nul_position()]).expect("prefix has no NUL"));
    unsafe {
        sys_calls::err_set_string(ts, exc_type, ctext.as_ptr());
        PyErrRaised::assume_set(ts)
    }
}

/// Convert a C `-1`-on-error return into a `PyResult`.
#[inline]
pub(crate) fn error_on_minus_one(ts: &ThreadState<'_>, ret: std::os::raw::c_int) -> PyResult<()> {
    if ret == -1 {
        Err(unsafe { PyErrRaised::assume_set(ts) })
    } else {
        Ok(())
    }
}

macro_rules! builtin_exceptions {
    ($($(#[$doc:meta])* $name:ident => $ffi_static:ident;)*) => {
        $(
            $(#[$doc])*
            pub struct $name;

            impl $name {
                /// Set this exception with the given message and return the
                /// raised marker.
                pub fn raise(ts: &ThreadState<'_>, msg: impl fmt::Display) -> PyErrRaised {
                    raise_with_type(ts, unsafe { ffi::$ffi_static }, msg)
                }
            }
        )*
    };
}

builtin_exceptions! {
    /// `Exception`
    PyException => PyExc_Exception;
    /// `ValueError`
    PyValueError => PyExc_ValueError;
    /// `TypeError`
    PyTypeError => PyExc_TypeError;
    /// `OverflowError`
    PyOverflowError => PyExc_OverflowError;
    /// `BufferError`
    PyBufferError => PyExc_BufferError;
    /// `EOFError`
    PyEOFError => PyExc_EOFError;
    /// `NotImplementedError`
    PyNotImplementedError => PyExc_NotImplementedError;
    /// `MemoryError`
    PyMemoryError => PyExc_MemoryError;
    /// `RuntimeError`
    PyRuntimeError => PyExc_RuntimeError;
    /// `SystemError`
    PySystemError => PyExc_SystemError;
}

impl<'py> Bound<'py, PyType> {
    /// Raise this exception type (e.g. a module-state-held `zlib.error`) with
    /// a formatted message.
    pub fn raise(&self, ts: &ThreadState<'py>, msg: impl fmt::Display) -> PyErrRaised {
        raise_with_type(ts, self.as_ptr(), msg)
    }
}

impl Py<PyType> {
    /// Raise this exception type (e.g. a module-state-held `zlib.error`) with
    /// a formatted message.
    pub fn raise(&self, ts: &ThreadState<'_>, msg: impl fmt::Display) -> PyErrRaised {
        raise_with_type(ts, self.as_ptr(), msg)
    }
}
