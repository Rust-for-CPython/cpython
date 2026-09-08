//! Conversions between Python objects and Rust values.

use std::os::raw::c_char;
use std::ptr::NonNull;

use crate::err::{PyErrRaised, PyOverflowError, PyResult};
use crate::ffi;
use crate::instance::{Bound, Py};
use crate::sys_calls;
use crate::threadstate::ThreadState;
use crate::types::PyAny;

/// Extract a Rust value from a Python object (argument conversion).
pub trait FromPyObject<'py>: Sized {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self>;
}

/// Convert a Rust value into a Python object (return-value conversion).
pub trait IntoPyObject<'py> {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>>;
}

#[inline]
fn none_ptr() -> *mut ffi::PyObject {
    (&raw const ffi::_Py_NoneStruct).cast_mut()
}

#[inline]
fn owned_or_raised<'py>(
    ts: &ThreadState<'py>,
    ptr: *mut ffi::PyObject,
) -> PyResult<Bound<'py, PyAny>> {
    match NonNull::new(ptr) {
        Some(p) => Ok(unsafe { Bound::from_owned_ptr(ts, p) }),
        None => Err(unsafe { PyErrRaised::assume_set(ts) }),
    }
}

// --- integers --------------------------------------------------------------

fn extract_i64(ts: &ThreadState<'_>, obj: &Bound<'_, PyAny>) -> PyResult<i64> {
    let v = unsafe { sys_calls::long_as_longlong(ts, obj.as_ptr()) };
    if v == -1 && ts.exception_set() {
        return Err(unsafe { PyErrRaised::assume_set(ts) });
    }
    Ok(v)
}

macro_rules! int_via_i64 {
    ($($ty:ty => $cname:literal;)*) => {
        $(
            impl<'py> FromPyObject<'py> for $ty {
                fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
                    let v = extract_i64(ts, obj)?;
                    <$ty>::try_from(v).map_err(|_| {
                        PyOverflowError::raise(
                            ts,
                            concat!("Python int out of range for C ", $cname),
                        )
                    })
                }
            }
        )*
    };
}

int_via_i64! {
    i32 => "int";
    u32 => "unsigned int";
    i64 => "long long";
}

impl<'py> FromPyObject<'py> for u64 {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
        let v = unsafe { sys_calls::long_as_unsigned_longlong(ts, obj.as_ptr()) };
        if v == u64::MAX && ts.exception_set() {
            return Err(unsafe { PyErrRaised::assume_set(ts) });
        }
        Ok(v)
    }
}

impl<'py> FromPyObject<'py> for isize {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
        let v = unsafe { sys_calls::long_as_ssize_t(ts, obj.as_ptr()) };
        if v == -1 && ts.exception_set() {
            return Err(unsafe { PyErrRaised::assume_set(ts) });
        }
        Ok(v as isize)
    }
}

impl<'py> FromPyObject<'py> for usize {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
        let v = unsafe { sys_calls::long_as_size_t(ts, obj.as_ptr()) };
        if v == usize::MAX && ts.exception_set() {
            return Err(unsafe { PyErrRaised::assume_set(ts) });
        }
        Ok(v)
    }
}

impl<'py> FromPyObject<'py> for bool {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
        let v = unsafe { sys_calls::object_is_true(ts, obj.as_ptr()) };
        if v == -1 {
            return Err(unsafe { PyErrRaised::assume_set(ts) });
        }
        Ok(v != 0)
    }
}

impl<'py, T: FromPyObject<'py>> FromPyObject<'py> for Option<T> {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
        if obj.as_ptr() == none_ptr() {
            Ok(None)
        } else {
            T::extract(ts, obj).map(Some)
        }
    }
}

impl<'py> FromPyObject<'py> for Bound<'py, PyAny> {
    fn extract(ts: &ThreadState<'py>, obj: &Bound<'py, PyAny>) -> PyResult<Self> {
        Ok(obj.clone_ref(ts))
    }
}

// --- IntoPyObject ----------------------------------------------------------

macro_rules! int_into_signed {
    ($($ty:ty),*) => {
        $(
            impl<'py> IntoPyObject<'py> for $ty {
                fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
                    let p = unsafe { sys_calls::long_from_longlong(ts, self as i64) };
                    owned_or_raised(ts, p)
                }
            }
        )*
    };
}

int_into_signed!(i8, i16, i32, i64);

macro_rules! int_into_unsigned {
    ($($ty:ty),*) => {
        $(
            impl<'py> IntoPyObject<'py> for $ty {
                fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
                    let p = unsafe { sys_calls::long_from_unsigned_longlong(ts, self as u64) };
                    owned_or_raised(ts, p)
                }
            }
        )*
    };
}

int_into_unsigned!(u8, u16, u32, u64);

impl<'py> IntoPyObject<'py> for isize {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        let p = unsafe { sys_calls::long_from_ssize_t(ts, self as ffi::Py_ssize_t) };
        owned_or_raised(ts, p)
    }
}

impl<'py> IntoPyObject<'py> for usize {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        let p = unsafe { sys_calls::long_from_unsigned_longlong(ts, self as u64) };
        owned_or_raised(ts, p)
    }
}

impl<'py> IntoPyObject<'py> for bool {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        let p = unsafe { sys_calls::bool_from_long(ts, self as std::os::raw::c_long) };
        owned_or_raised(ts, p)
    }
}

impl<'py> IntoPyObject<'py> for &str {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        let p = unsafe {
            sys_calls::unicode_from_string_and_size(
                ts,
                self.as_ptr() as *const c_char,
                self.len() as ffi::Py_ssize_t,
            )
        };
        owned_or_raised(ts, p)
    }
}

impl<'py> IntoPyObject<'py> for String {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.as_str().into_pyobject(ts)
    }
}

impl<'py> IntoPyObject<'py> for () {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        // None is immortal; the incref is a no-op but keeps the ownership
        // model uniform.
        let none = none_ptr();
        unsafe {
            sys_calls::incref(ts, none);
            Ok(Bound::from_owned_ptr(ts, NonNull::new_unchecked(none)))
        }
    }
}

impl<'py, T> IntoPyObject<'py> for Bound<'py, T> {
    fn into_pyobject(self, _ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(self.into_any())
    }
}

impl<'py, T> IntoPyObject<'py> for Py<T> {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(self.into_bound(ts).into_any())
    }
}

impl<'py, T: IntoPyObject<'py>> IntoPyObject<'py> for Option<T> {
    fn into_pyobject(self, ts: &ThreadState<'py>) -> PyResult<Bound<'py, PyAny>> {
        match self {
            Some(v) => v.into_pyobject(ts),
            None => ().into_pyobject(ts),
        }
    }
}

// --- trampoline return conversion ------------------------------------------

/// Return-value conversion for generated trampolines: accepts both plain
/// values and `PyResult`s of them.
#[doc(hidden)]
pub trait IntoPyCallbackOutput<'py> {
    fn convert(self, ts: &ThreadState<'py>) -> PyResult<*mut ffi::PyObject>;
}

impl<'py, T: IntoPyObject<'py>> IntoPyCallbackOutput<'py> for T {
    fn convert(self, ts: &ThreadState<'py>) -> PyResult<*mut ffi::PyObject> {
        Ok(self.into_pyobject(ts)?.into_ptr())
    }
}

impl<'py, T: IntoPyObject<'py>> IntoPyCallbackOutput<'py> for PyResult<T> {
    fn convert(self, ts: &ThreadState<'py>) -> PyResult<*mut ffi::PyObject> {
        Ok(self?.into_pyobject(ts)?.into_ptr())
    }
}
