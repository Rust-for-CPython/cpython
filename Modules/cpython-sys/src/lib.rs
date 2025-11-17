#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unnecessary_transmutes)]

use std::ffi::{c_char, c_int, c_void};

include!(concat!(env!("OUT_DIR"), "/c_api.rs"));

// TODO(emmatyping): include parser bindings (see build.rs)
//include!(concat!(env!("OUT_DIR"), "/parser.rs"));
/* Flag passed to newmethodobject */
/* #define METH_OLDARGS  0x0000   -- unsupported now */
pub const METH_VARARGS: c_int = 0x0001;
pub const METH_KEYWORDS: c_int = 0x0002;
/* METH_NOARGS and METH_O must not be combined with the flags above. */
pub const METH_NOARGS: c_int = 0x0004;
pub const METH_O: c_int = 0x0008;

/* METH_CLASS and METH_STATIC are a little different; these control
the construction of methods for a class.  These cannot be used for
functions in modules. */
pub const METH_CLASS: c_int = 0x0010;
pub const METH_STATIC: c_int = 0x0020;

/* METH_COEXIST allows a method to be entered even though a slot has
already filled the entry.  When defined, the flag allows a separate
method, "__contains__" for example, to coexist with a defined
slot like sq_contains. */

pub const METH_COEXIST: c_int = 0x0040;

pub const METH_FASTCALL: c_int = 0x0080;

/* This bit is preserved for Stackless Python
pub const METH_STACKLESS: c_int = 0x0100;
pub const METH_STACKLESS: c_int = 0x0000;
*/

/* METH_METHOD means the function stores an
 * additional reference to the class that defines it;
 * both self and class are passed to it.
 * It uses PyCMethodObject instead of PyCFunctionObject.
 * May not be combined with METH_NOARGS, METH_O, METH_CLASS or METH_STATIC.
 */

pub const METH_METHOD: c_int = 0x0200;

#[cfg(target_pointer_width = "64")]
pub const _Py_STATIC_FLAG_BITS: Py_ssize_t =
    (_Py_STATICALLY_ALLOCATED_FLAG | _Py_IMMORTAL_FLAGS) as Py_ssize_t;
#[cfg(target_pointer_width = "64")]
pub const _Py_STATIC_IMMORTAL_INITIAL_REFCNT: Py_ssize_t =
    (_Py_IMMORTAL_INITIAL_REFCNT as Py_ssize_t) | (_Py_STATIC_FLAG_BITS << 48);
#[cfg(not(target_pointer_width = "64"))]
pub const _Py_STATIC_IMMORTAL_INITIAL_REFCNT: Py_ssize_t = 7u32 << 28;

#[repr(C)]
pub union PyMethodDefFuncPointer {
    pub PyCFunction: unsafe extern "C" fn(slf: *mut PyObject, args: *mut PyObject) -> *mut PyObject,
    pub PyCFunctionFast: unsafe extern "C" fn(
        slf: *mut PyObject,
        args: *mut *mut PyObject,
        nargs: Py_ssize_t,
    ) -> *mut PyObject,
    pub PyCFunctionWithKeywords: unsafe extern "C" fn(
        slf: *mut PyObject,
        args: *mut PyObject,
        kwargs: *mut PyObject,
    ) -> *mut PyObject,
    pub PyCFunctionFastWithKeywords: unsafe extern "C" fn(
        slf: *mut PyObject,
        args: *mut *mut PyObject,
        nargs: Py_ssize_t,
        kwargs: *mut PyObject,
    ) -> *mut PyObject,
    pub PyCMethod: unsafe extern "C" fn(
        slf: *mut PyObject,
        typ: *mut PyTypeObject,
        args: *mut *mut PyObject,
        nargs: Py_ssize_t,
        kwargs: *mut PyObject,
    ) -> *mut PyObject,
    pub Void: *mut c_void,
}

#[repr(C)]
pub struct PyMethodDef {
    pub ml_name: *mut c_char,
    pub ml_meth: PyMethodDefFuncPointer,
    pub ml_flags: c_int,
    pub ml_doc: *mut c_char,
}

impl PyMethodDef {
    pub const fn zeroed() -> Self {
        Self {
            ml_name: std::ptr::null_mut(),
            ml_meth: PyMethodDefFuncPointer {
                Void: std::ptr::null_mut(),
            },
            ml_flags: 0,
            ml_doc: std::ptr::null_mut(),
        }
    }
}

// TODO: this is pretty unsafe, we should probably wrap this in a nicer
// abstraction
unsafe impl Sync for PyMethodDef {}
unsafe impl Send for PyMethodDef {}

#[cfg(py_gil_disabled)]
pub const PyObject_HEAD_INIT: PyObject = {
    let mut obj: PyObject = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    obj.ob_flags = _Py_STATICALLY_ALLOCATED_FLAG as _;
    obj
};

#[cfg(not(py_gil_disabled))]
pub const PyObject_HEAD_INIT: PyObject = PyObject {
    __bindgen_anon_1: _object__bindgen_ty_1 {
        ob_refcnt_full: _Py_STATIC_IMMORTAL_INITIAL_REFCNT as i64,
    },
    ob_type: std::ptr::null_mut(),
};

pub const PyModuleDef_HEAD_INIT: PyModuleDef_Base = PyModuleDef_Base {
    ob_base: PyObject_HEAD_INIT,
    m_init: None,
    m_index: 0,
    m_copy: std::ptr::null_mut(),
};
