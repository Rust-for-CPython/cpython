use std::cell::UnsafeCell;

use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;

use cpython_sys::METH_FASTCALL;
use cpython_sys::Py_ssize_t;
use cpython_sys::PyBytes_AsString;
use cpython_sys::PyBytes_FromString;
use cpython_sys::PyMethodDef;
use cpython_sys::PyMethodDefFuncPointer;
use cpython_sys::PyModuleDef;
use cpython_sys::PyModuleDef_HEAD_INIT;
use cpython_sys::PyModuleDef_Init;
use cpython_sys::PyObject;

use base64::prelude::*;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn standard_b64encode(
    _module: *mut PyObject,
    args: *mut *mut PyObject,
    _nargs: Py_ssize_t,
) -> *mut PyObject {
    let buff = unsafe { *args };
    let ptr = unsafe { PyBytes_AsString(buff) };
    if ptr.is_null() {
        // Error handling omitted for now
        unimplemented!("Error handling goes here...")
    }
    let cdata = unsafe { CStr::from_ptr(ptr) };
    let res = BASE64_STANDARD.encode(cdata.to_bytes());
    unsafe { PyBytes_FromString(CString::new(res).unwrap().as_ptr()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn _base64_clear(_obj: *mut PyObject) -> c_int {
    //TODO
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn _base64_free(_o: *mut c_void) {
    //TODO
}

pub struct ModuleDef {
    ffi: UnsafeCell<PyModuleDef>,
}

impl ModuleDef {
    fn init_multi_phase(&'static self) -> *mut PyObject {
        unsafe { PyModuleDef_Init(self.ffi.get()) }
    }
}

unsafe impl Sync for ModuleDef {}

pub static _BASE64_MODULE_METHODS: [PyMethodDef; 2] = {
    [
        PyMethodDef {
            ml_name: c"standard_b64encode".as_ptr() as *mut c_char,
            ml_meth: PyMethodDefFuncPointer {
                PyCFunctionFast: standard_b64encode,
            },
            ml_flags: METH_FASTCALL,
            ml_doc: c"Demo for the _base64 module".as_ptr() as *mut c_char,
        },
        PyMethodDef::zeroed(),
    ]
};

pub static _BASE64_MODULE: ModuleDef = {
    ModuleDef {
        ffi: UnsafeCell::new(PyModuleDef {
            m_base: PyModuleDef_HEAD_INIT,
            m_name: c"_base64".as_ptr() as *mut _,
            m_doc: c"A test Rust module".as_ptr() as *mut _,
            m_size: 0,
            m_methods: &_BASE64_MODULE_METHODS as *const PyMethodDef as *mut _,
            m_slots: std::ptr::null_mut(),
            m_traverse: None,
            m_clear: Some(_base64_clear),
            m_free: Some(_base64_free),
        }),
    }
};

#[unsafe(no_mangle)]
pub extern "C" fn PyInit__base64() -> *mut PyObject {
    _BASE64_MODULE.init_multi_phase()
}
