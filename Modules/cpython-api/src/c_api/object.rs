use std::ffi::c_int;

use cpython_sys::PyTypeObject;

use crate::PyAny;

pub(crate) unsafe fn Py_TYPE(ob: *mut PyAny) -> *mut PyTypeObject {
    todo!()
}

pub(crate) unsafe fn Py_IsNone(x: *mut PyAny) -> c_int {
    todo!()
}
