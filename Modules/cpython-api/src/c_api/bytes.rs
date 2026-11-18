use std::ffi::c_char;

use cpython_sys::Py_ssize_t;

use crate::PyAny;

pub(crate) unsafe fn PyBytes_FromStringAndSize(v: *const c_char, len: Py_ssize_t) -> *mut PyAny {
    todo!()
}

pub(crate) unsafe fn PyBytes_AsString(o: *mut PyAny) -> *mut c_char {
    todo!()
}

pub(crate) unsafe fn PyBytes_Size(o: *mut PyAny) -> Py_ssize_t {
    todo!()
}
