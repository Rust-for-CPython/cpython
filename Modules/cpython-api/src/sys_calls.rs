//! The single raw-call layer.
//!
//! Every C API call made by this crate goes through one of these wrappers so
//! the explicit-tstate migration is mechanical and auditable. Each wrapper is
//! annotated:
//!
//! - `explicit-tstate`: the underlying C API takes the thread state
//!   explicitly (or we read a `PyThreadState` field directly).
//! - `tls-fallback`: the underlying C API fetches the thread state from TLS.
//!   These are temporary: per the design (RUST_API.md, "Explicit-tstate C API
//!   policy"), each one is a candidate for a thin `_Py*` internal wrapper in
//!   CPython that takes `tstate` explicitly. Holding `&ThreadState` while
//!   calling them keeps them sound in the meantime (the token proves the TLS
//!   state is ours).
//!
//! All functions here are `unsafe`: they trust raw pointers from the caller.
//! The `ts` parameter both proves attachment and (where possible) supplies
//! the tstate pointer.

use std::os::raw::{c_char, c_int, c_void};

use crate::ffi;
use crate::threadstate::ThreadState;

// --- refcounting -----------------------------------------------------------

/// explicit-attachment (tls-fallback internally on free-threaded builds,
/// where biased refcounting consults the thread id).
#[inline]
pub(crate) unsafe fn incref(_ts: &ThreadState<'_>, obj: *mut ffi::PyObject) {
    unsafe { ffi::Py_IncRef(obj) }
}

/// explicit-attachment (see `incref`).
#[inline]
pub(crate) unsafe fn decref(_ts: &ThreadState<'_>, obj: *mut ffi::PyObject) {
    unsafe { ffi::Py_DecRef(obj) }
}

// --- exceptions ------------------------------------------------------------

/// explicit-tstate: `_PyErr_SetString(tstate, ...)`.
#[inline]
pub(crate) unsafe fn err_set_string(
    ts: &ThreadState<'_>,
    exc_type: *mut ffi::PyObject,
    msg: *const c_char,
) {
    unsafe { ffi::_PyErr_SetString(ts.as_ptr(), exc_type, msg) }
}

/// tls-fallback: `PyErr_NewException`.
#[inline]
pub(crate) unsafe fn err_new_exception(
    _ts: &ThreadState<'_>,
    qualname: *const c_char,
    base: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyErr_NewException(qualname, base, std::ptr::null_mut()) }
}

// --- bytes -----------------------------------------------------------------

/// tls-fallback: `PyBytes_FromStringAndSize` (candidate for an explicit-
/// tstate `_Py*` wrapper; allocation paths consult TLS).
#[inline]
pub(crate) unsafe fn bytes_from_string_and_size(
    _ts: &ThreadState<'_>,
    data: *const c_char,
    len: ffi::Py_ssize_t,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyBytes_FromStringAndSize(data, len) }
}

/// tls-fallback: `PyBytes_AsString`.
#[inline]
pub(crate) unsafe fn bytes_as_string(
    _ts: &ThreadState<'_>,
    obj: *mut ffi::PyObject,
) -> *mut c_char {
    unsafe { ffi::PyBytes_AsString(obj) }
}

// --- buffers ---------------------------------------------------------------

/// tls-fallback: `PyObject_GetBuffer`.
#[inline]
pub(crate) unsafe fn get_buffer(
    _ts: &ThreadState<'_>,
    obj: *mut ffi::PyObject,
    view: *mut ffi::Py_buffer,
    flags: c_int,
) -> c_int {
    unsafe { ffi::PyObject_GetBuffer(obj, view, flags) }
}

// --- int / bool / str conversions -----------------------------------------

/// tls-fallback: `PyLong_AsLongLong`.
#[inline]
pub(crate) unsafe fn long_as_longlong(_ts: &ThreadState<'_>, obj: *mut ffi::PyObject) -> i64 {
    unsafe { ffi::PyLong_AsLongLong(obj) }
}

/// tls-fallback: `PyLong_AsUnsignedLongLong`.
#[inline]
pub(crate) unsafe fn long_as_unsigned_longlong(
    _ts: &ThreadState<'_>,
    obj: *mut ffi::PyObject,
) -> u64 {
    unsafe { ffi::PyLong_AsUnsignedLongLong(obj) }
}

/// tls-fallback: `PyLong_AsSsize_t`.
#[inline]
pub(crate) unsafe fn long_as_ssize_t(
    _ts: &ThreadState<'_>,
    obj: *mut ffi::PyObject,
) -> ffi::Py_ssize_t {
    unsafe { ffi::PyLong_AsSsize_t(obj) }
}

/// tls-fallback: `PyLong_AsSize_t`.
#[inline]
pub(crate) unsafe fn long_as_size_t(_ts: &ThreadState<'_>, obj: *mut ffi::PyObject) -> usize {
    unsafe { ffi::PyLong_AsSize_t(obj) }
}

/// tls-fallback: `PyLong_FromLongLong`.
#[inline]
pub(crate) unsafe fn long_from_longlong(_ts: &ThreadState<'_>, v: i64) -> *mut ffi::PyObject {
    unsafe { ffi::PyLong_FromLongLong(v) }
}

/// tls-fallback: `PyLong_FromUnsignedLongLong`.
#[inline]
pub(crate) unsafe fn long_from_unsigned_longlong(
    _ts: &ThreadState<'_>,
    v: u64,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyLong_FromUnsignedLongLong(v) }
}

/// tls-fallback: `PyLong_FromSsize_t`.
#[inline]
pub(crate) unsafe fn long_from_ssize_t(
    _ts: &ThreadState<'_>,
    v: ffi::Py_ssize_t,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyLong_FromSsize_t(v) }
}

/// tls-fallback: `PyObject_IsTrue`.
#[inline]
pub(crate) unsafe fn object_is_true(_ts: &ThreadState<'_>, obj: *mut ffi::PyObject) -> c_int {
    unsafe { ffi::PyObject_IsTrue(obj) }
}

/// tls-fallback: `PyBool_FromLong`.
#[inline]
pub(crate) unsafe fn bool_from_long(
    _ts: &ThreadState<'_>,
    v: std::os::raw::c_long,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyBool_FromLong(v) }
}

/// tls-fallback: `PyUnicode_FromStringAndSize`.
#[inline]
pub(crate) unsafe fn unicode_from_string_and_size(
    _ts: &ThreadState<'_>,
    data: *const c_char,
    len: ffi::Py_ssize_t,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyUnicode_FromStringAndSize(data, len) }
}

/// explicit-tstate-free: pure comparison, no tstate involved.
#[inline]
pub(crate) unsafe fn unicode_eq_ascii(obj: *mut ffi::PyObject, s: *const c_char) -> c_int {
    unsafe { ffi::_PyUnicode_EqualToASCIIString(obj, s) }
}

// --- tuples / dicts (argument parsing) -------------------------------------

/// tls-fallback: `PyTuple_Size`.
#[inline]
pub(crate) unsafe fn tuple_size(_ts: &ThreadState<'_>, t: *mut ffi::PyObject) -> ffi::Py_ssize_t {
    unsafe { ffi::PyTuple_Size(t) }
}

/// tls-fallback: `PyTuple_GetItem` (borrowed reference).
#[inline]
pub(crate) unsafe fn tuple_get_item(
    _ts: &ThreadState<'_>,
    t: *mut ffi::PyObject,
    i: ffi::Py_ssize_t,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyTuple_GetItem(t, i) }
}

/// tls-fallback: `PyDict_Size`.
#[inline]
pub(crate) unsafe fn dict_size(_ts: &ThreadState<'_>, d: *mut ffi::PyObject) -> ffi::Py_ssize_t {
    unsafe { ffi::PyDict_Size(d) }
}

/// tls-fallback: `PyDict_GetItemStringRef` (strong reference out-param).
#[inline]
pub(crate) unsafe fn dict_get_item_string_ref(
    _ts: &ThreadState<'_>,
    d: *mut ffi::PyObject,
    key: *const c_char,
    out: *mut *mut ffi::PyObject,
) -> c_int {
    unsafe { ffi::PyDict_GetItemStringRef(d, key, out) }
}

// --- modules ---------------------------------------------------------------

/// tls-fallback: `PyModule_AddObjectRef`.
#[inline]
pub(crate) unsafe fn module_add_object_ref(
    _ts: &ThreadState<'_>,
    m: *mut ffi::PyObject,
    name: *const c_char,
    value: *mut ffi::PyObject,
) -> c_int {
    unsafe { ffi::PyModule_AddObjectRef(m, name, value) }
}

/// tls-fallback: `PyModule_AddType`.
#[inline]
pub(crate) unsafe fn module_add_type(
    _ts: &ThreadState<'_>,
    m: *mut ffi::PyObject,
    ty: *mut ffi::PyTypeObject,
) -> c_int {
    unsafe { ffi::PyModule_AddType(m, ty) }
}

/// tstate-free: reads the module object.
#[inline]
pub(crate) unsafe fn module_get_state(m: *mut ffi::PyObject) -> *mut c_void {
    unsafe { ffi::PyModule_GetState(m) }
}

// --- types -----------------------------------------------------------------

/// tls-fallback: `PyType_FromSlots`.
#[inline]
pub(crate) unsafe fn type_from_slots(
    _ts: &ThreadState<'_>,
    slots: *mut ffi::PySlot,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyType_FromSlots(slots) }
}

/// tls-fallback: `PyType_GenericAlloc`.
#[inline]
pub(crate) unsafe fn type_generic_alloc(
    _ts: &ThreadState<'_>,
    tp: *mut ffi::PyTypeObject,
    nitems: ffi::Py_ssize_t,
) -> *mut ffi::PyObject {
    unsafe { ffi::PyType_GenericAlloc(tp, nitems) }
}

/// tstate-free: reads the type object.
#[inline]
pub(crate) unsafe fn type_get_module_state(tp: *mut ffi::PyTypeObject) -> *mut c_void {
    unsafe { ffi::PyType_GetModuleState(tp) }
}
