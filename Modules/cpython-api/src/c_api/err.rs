use std::ffi::c_char;

use crate::PyAny;

pub(crate) unsafe fn PyErr_SetString(exception: *mut PyAny, message: *const c_char) {
    todo!()
}

pub(crate) unsafe fn PyErr_NoMemory() -> *mut PyAny {
    todo!()
}

pub(crate) unsafe fn PyErr_Occurred() -> *mut PyAny {
    todo!()
}
